//! The RSA / MPI accelerator (`specs/blocks/rsa.toml`, TRM chapter 20). Nothing on the boot path
//! uses it; mbedTLS reaches it for every big-number product and exponentiation of a TLS handshake
//! (IDF `mbedtls/port/bignum/esp_bignum.c`) and for WPA3-SAE.
//!
//! `rsa.query_clean` and `rsa.query_interrupt` loop while the bit is 0, so a store-only model
//! would hang the guest; both reads are modeled.
//!
//! Operands are little-endian arrays of 32-bit words, [`reg::LENGTH`] long. `MODEXP_START` is
//! `Z = X ^ Y mod M`, with `SEARCH_ENABLE` ignoring exponent bits above `SEARCH_POS` (TRM 20.3.4);
//! `MOD_MULT_START` is `Z = X * Y mod M`; `MULT_START` is `Z = X * Y` laid out as
//! `esp_mpi_mul_mpi_hw_op` loads it ([`Rsa::run_mult`]).
//!
//! The block is a Montgomery pipeline and computes with the `M_PRIME` and `r` the caller loaded,
//! as silicon does: the `probe_campaign_timing` capture shows `M_PRIME = 0` answering 1 where the
//! right value answers the residue (class A). Cases the TRM does not state are UNVERIFIED.
//!
//! Time: an operation is a count of Montgomery multiplications ([`Rsa::op_ps`]), each `rsa_op_ps`
//! (one at 64 words) scaled by the square of the word count. Class A for the exponentiation: one
//! multiplication of 8583.1 cycles reproduces the three captured 2048-bit cases within 0.012 %.
//! The TRM's table 20.3-1 example comes out 1.5 to 3.9 % short under this rule. The modular
//! multiplication (2 multiplications) and plain multiplication (1) are class C.

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::regstore::Size;
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::{Cx, Peripheral, RegRead, RegWrite, Stability, Wiring};

/// Block offsets (TRM 20.4 and 20.5).
pub mod reg {
    /// Modulus operand memory.
    pub const M: u32 = 0x000;
    /// Result operand memory; the caller loads `Rinv` here before a start.
    pub const Z: u32 = 0x200;
    /// Exponent or second factor.
    pub const Y: u32 = 0x400;
    /// Base or first factor.
    pub const X: u32 = 0x600;
    pub const MEM_END: u32 = 0x800;
    /// Montgomery parameter of the modulus.
    pub const M_PRIME: u32 = 0x800;
    /// Operand word count minus one for the two modular operations, the *result* word count
    /// minus one for the plain multiplication (the TRM's `RSA_MODE_REG`, bits 6 to 0).
    pub const LENGTH: u32 = 0x804;
    /// 1 once the operand memory is powered up.
    pub const QUERY_CLEAN: u32 = 0x808;
    /// Start `Z = X^Y mod M`.
    pub const MODEXP_START: u32 = 0x80C;
    /// Start `Z = X * Y mod M`.
    pub const MOD_MULT_START: u32 = 0x810;
    /// Start `Z = X * Y`, the plain multiplication.
    pub const MULT_START: u32 = 0x814;
    /// 1 once the operation is done.
    pub const QUERY_INTERRUPT: u32 = 0x818;
    pub const CLEAR_INTERRUPT: u32 = 0x81C;
    pub const CONSTANT_TIME: u32 = 0x820;
    pub const SEARCH_ENABLE: u32 = 0x824;
    pub const SEARCH_POS: u32 = 0x828;
    pub const INT_ENA: u32 = 0x82C;
    /// Block version (TRM register 20.13).
    pub const DATE: u32 = 0x830;
}

pub const WINDOW_WORDS: usize = 0x200 / 4;

/// Longest operand the block accepts: 3072 bits (TRM 20.3.1, 96 words per memory block).
pub const MAX_WORDS: usize = 3072 / 32;

/// `LENGTH` bits 6 to 0 (TRM register 20.2).
const LENGTH_MASK: u32 = 0x7F;

/// `SEARCH_POS` bits 11 to 0 (TRM register 20.11).
const SEARCH_POS_MASK: u32 = 0xFFF;

/// `DATE` at reset (TRM register 20.13).
pub const DATE_RESET: u32 = 0x2020_0618;

pub const TAG_DONE: u16 = 0;

/// Picoseconds a `CONSTANT_TIME` 0 exponentiation spends per zero bit above the exponent's top
/// set bit: TRM table 20.3-1, 2.406 ms without search against 2.33 ms with it over the 3055 bits
/// it skips (class B). Charged only when [`Rsa::set_op_ps`] is nonzero.
pub const SKIP_BIT_PS: u64 = 25_000;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Rsa {
    /// `M`, `Z`, `Y` and `X`, in that offset order, one [`WINDOW_WORDS`] window each.
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, { WINDOW_WORDS * 4 }>")]
    mem: Vec<u32>,
    m_prime: u32,
    length: u32,
    constant_time: u32,
    search_enable: u32,
    search_pos: u32,
    int_ena: u32,
    date: u32,
    busy: bool,
    /// The completion interrupt, latched until `CLEAR_INTERRUPT` ([`Rsa::complete`]).
    irq_raw: bool,
    op_ps: u64,
    #[serde(
        deserialize_with = "pemu_core::snap::exact_vec::<_, _, { (Rsa::SIZE.div_ceil(4) as usize).div_ceil(64) }>"
    )]
    touched: Vec<u64>,
}

impl Default for Rsa {
    fn default() -> Rsa {
        Rsa {
            mem: vec![0; WINDOW_WORDS * 4],
            m_prime: 0,
            length: 0,
            // The constant-time option and the interrupt are on at reset (TRM 20.9, 20.12).
            constant_time: 1,
            search_enable: 0,
            search_pos: 0,
            int_ena: 1,
            date: DATE_RESET,
            busy: false,
            irq_raw: false,
            op_ps: 0,
            touched: vec![0; (Rsa::SIZE.div_ceil(4) as usize).div_ceil(64)],
        }
    }
}

impl Rsa {
    /// Whether an operation is still in progress, which is `QUERY_INTERRUPT` reading 0.
    pub fn busy(&self) -> bool {
        self.busy
    }

    /// Picoseconds one 64-word Montgomery multiplication takes, set from `rsa_op_ps`.
    pub fn set_op_ps(&mut self, ps: u64) {
        self.op_ps = ps;
    }

    pub fn result(&self) -> &[u32] {
        self.window(reg::Z)
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        self.touch(off, TouchAccess::Read, size, now, ledger);
        let (mask, shift) = window_bits(off, size);
        (self.word(off & !3) & mask) >> shift
    }

    /// Writes the low `size` bytes of `val` at `off` and runs whatever a start trigger began.
    /// Returns whether the write scheduled a completion, so `QUERY_INTERRUPT` cannot set a slice
    /// late.
    #[must_use]
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
        sched: &mut Scheduler,
    ) -> bool {
        self.touch(off, TouchAccess::Write, size, now, ledger);
        let aligned = off & !3;
        let (mask, shift) = window_bits(off, size);
        let written = ((u64::from(val) << shift) as u32) & mask;
        let merged = (self.word(aligned) & !mask) | written;
        let mut scheduled = false;
        match aligned {
            reg::M..reg::MEM_END => self.mem[(aligned / 4) as usize] = merged,
            reg::M_PRIME => self.m_prime = merged,
            reg::LENGTH => self.length = merged & LENGTH_MASK,
            reg::CLEAR_INTERRUPT if written & 1 != 0 => self.irq_raw = false,
            reg::CONSTANT_TIME => self.constant_time = merged & 1,
            reg::SEARCH_ENABLE => self.search_enable = merged & 1,
            reg::SEARCH_POS => self.search_pos = merged & SEARCH_POS_MASK,
            reg::INT_ENA => self.int_ena = merged & 1,
            reg::DATE => self.date = merged,
            reg::MODEXP_START | reg::MOD_MULT_START | reg::MULT_START if written & 1 != 0 => {
                // The time is taken from the operands before the operation overwrites any.
                let op_ps = self.op_ps(aligned);
                match aligned {
                    reg::MODEXP_START => self.run_modular(true),
                    reg::MOD_MULT_START => self.run_modular(false),
                    _ => self.run_mult(),
                }
                self.busy = true;
                // The result is ready, but QUERY_INTERRUPT is state the guest polls, so even an
                // immediate completion is an event at `now`.
                sched.schedule(
                    now,
                    VTime(now.0.saturating_add(op_ps)),
                    EventKey {
                        owner: Owner::Periph(Rsa::ID),
                        tag: TAG_DONE,
                    },
                );
                scheduled = true;
            }
            _ => {}
        }
        scheduled
    }

    /// The completion: `QUERY_INTERRUPT` reads 1 again and the interrupt latches. One that finds
    /// the block idle was overtaken by a reset (the scheduler does not cancel it).
    ///
    /// Source 47 follows the latch while `INT_ENA` bit 0 is set, until `CLEAR_INTERRUPT`.
    /// `QUERY_INTERRUPT` does not depend on the latch: IDF's ISR clears it before
    /// `mpi_hal_wait_op_complete` polls the idle bit.
    pub fn complete(&mut self) {
        if self.busy {
            self.busy = false;
            self.irq_raw = true;
        }
    }

    /// Picoseconds the operation `start` takes on the operands loaded now. With `t` the exponent's
    /// top set bit among the considered bits and `h` its set bits, an exponentiation takes `2L`
    /// multiplications for `L` considered bits with `CONSTANT_TIME` 1, and `t + h - 1` plus
    /// [`SKIP_BIT_PS`] per skipped leading zero with 0; both add 2 for the Montgomery
    /// conversions.
    fn op_ps(&self, start: u32) -> u64 {
        if self.op_ps == 0 {
            return 0;
        }
        let (words, mults, skipped) = match start {
            reg::MODEXP_START => {
                let n = self.words();
                let y = self.window(reg::Y);
                let top = 32 * n - 1;
                let last = if self.search_enable & 1 != 0 {
                    (self.search_pos as usize).min(top)
                } else {
                    top
                };
                let bit = |i: usize| y[i / 32] >> (i % 32) & 1 == 1;
                let considered = last as u64 + 1;
                if self.constant_time & 1 != 0 {
                    (n, 2 * considered + 2, 0)
                } else {
                    match (0..=last).rev().find(|&i| bit(i)) {
                        Some(t) => {
                            let h = (0..=t).filter(|&i| bit(i)).count() as u64;
                            (n, t as u64 + h - 1 + 2, (last - t) as u64)
                        }
                        None => (n, 2, considered),
                    }
                }
            }
            reg::MOD_MULT_START => (self.words(), 2, 0),
            _ => (self.words().div_ceil(2).min(MAX_WORDS / 2), 1, 0),
        };
        let words = words as u128;
        let ps = u128::from(self.op_ps) * words * words * u128::from(mults) / (64 * 64)
            + u128::from(SKIP_BIT_PS) * u128::from(skipped);
        u64::try_from(ps).unwrap_or(u64::MAX)
    }

    /// Level of source 47: the latched completion while `INT_ENA` bit 0 is set.
    pub fn irq_level(&self) -> bool {
        self.irq_raw && self.int_ena & 1 != 0
    }

    pub fn sync_irq(&self, irq: &mut crate::intc::IrqFabric) {
        irq.set_source(pemu_core::irq_source::irq::RSA, self.irq_level());
    }

    /// Clears the block for a reset that reaches it, keeping the timing profile and the
    /// first-touch state.
    pub fn apply_reset(&mut self, kind: ResetKind) {
        if !kind.clears(pemu_core::regstore::RESET_BY_ALL_SCOPES) {
            return;
        }
        let touched = std::mem::take(&mut self.touched);
        let op_ps = self.op_ps;
        *self = Rsa {
            touched,
            op_ps,
            ..Rsa::default()
        };
    }

    fn word(&self, at: u32) -> u32 {
        match at {
            reg::M..reg::MEM_END => self.mem[(at / 4) as usize],
            reg::M_PRIME => self.m_prime,
            reg::LENGTH => self.length,
            // The operand memory of this model is always powered up.
            reg::QUERY_CLEAN => 1,
            reg::QUERY_INTERRUPT => u32::from(!self.busy),
            reg::CONSTANT_TIME => self.constant_time,
            reg::SEARCH_ENABLE => self.search_enable,
            reg::SEARCH_POS => self.search_pos,
            reg::INT_ENA => self.int_ena,
            reg::DATE => self.date,
            // Start triggers, CLEAR_INTERRUPT and unnamed offsets read 0.
            _ => 0,
        }
    }

    fn window(&self, base: u32) -> &[u32] {
        let start = (base / 4) as usize;
        &self.mem[start..start + self.words()]
    }

    /// Operand word count: `LENGTH + 1`, clamped to the block's 96.
    fn words(&self) -> usize {
        ((self.length as usize).saturating_add(1)).clamp(1, MAX_WORDS)
    }

    /// Runs `Z = X^Y mod M` or `Z = X * Y mod M`. An even modulus, which the Montgomery pipeline
    /// cannot take, leaves `Z` unchanged, and the other memories stay as they were (both
    /// UNVERIFIED).
    fn run_modular(&mut self, exponentiate: bool) {
        let n = self.words();
        let m = self.window(reg::M).to_vec();
        let x = self.window(reg::X).to_vec();
        let mut y = self.window(reg::Y).to_vec();
        if m[0] & 1 == 0 {
            return;
        }
        // The pipeline computes with the caller's `r` and `M'` as given.
        let r = self.window(reg::Z).to_vec();
        let n_prime = self.m_prime;
        let z = if exponentiate {
            // With the search option the bits of Y above SEARCH_POS are ignored.
            if self.search_enable & 1 != 0 {
                truncate_above(&mut y, self.search_pos as usize);
            }
            mod_exp(&x, &y, &m, &r, n_prime)
        } else {
            mod_mul(&x, &y, &m, &r, n_prime)
        };
        let start = (reg::Z / 4) as usize;
        self.mem[start..start + n].copy_from_slice(&z);
    }

    /// Runs `Z = X * Y`: `LENGTH + 1` is the result length `2n`, `X` the first `n` words of
    /// `X_MEM`, `Y` words `n` to `2n - 1` of `Z_MEM`. An odd result length takes `n` rounded up
    /// (UNVERIFIED).
    fn run_mult(&mut self) {
        let n = self.words().div_ceil(2).min(MAX_WORDS / 2);
        let x = self.window_of(reg::X, 0, n);
        let y = self.window_of(reg::Z, n, n);
        let product = mul(&x, &y);
        let start = (reg::Z / 4) as usize;
        self.mem[start..start + 2 * n].copy_from_slice(&product);
    }

    /// `len` words of the memory block at `base`, from word `from`.
    fn window_of(&self, base: u32, from: usize, len: usize) -> Vec<u32> {
        let start = (base / 4) as usize + from;
        self.mem[start..start + len].to_vec()
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
            periph: Rsa::ID,
            off: off & !3,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }
}

/// Mask and shift of an access of `size` bytes at `off` inside its 32-bit register.
fn window_bits(off: u32, size: Size) -> (u32, u32) {
    let shift = (off % 4) * 8;
    let bits = (size as u32) * 8;
    let mask = (((1u64 << bits) - 1) << shift) as u32;
    (mask, shift)
}

/// Clears every bit of `y` above bit `pos`, little-endian words.
fn truncate_above(y: &mut [u32], pos: usize) {
    for (i, word) in y.iter_mut().enumerate() {
        let low = 32 * i;
        if low > pos {
            *word = 0;
        } else if pos - low < 31 {
            *word &= (1u32 << (pos - low + 1)) - 1;
        }
    }
}

/// `x * y`, both little-endian words, as `x.len() + y.len()` words.
fn mul(x: &[u32], y: &[u32]) -> Vec<u32> {
    let mut out = vec![0u32; x.len() + y.len()];
    for (i, a) in x.iter().enumerate() {
        let mut carry = 0u64;
        for (j, b) in y.iter().enumerate() {
            let sum = u64::from(out[i + j]) + u64::from(*a) * u64::from(*b) + carry;
            out[i + j] = sum as u32;
            carry = sum >> 32;
        }
        out[i + y.len()] = carry as u32;
    }
    out
}

/// `-m^-1 mod 2^32` for an odd `m`, by Newton iteration: `M_PRIME` as IDF `modular_inverse`
/// computes it.
#[cfg(test)]
fn mont_n_prime(m0: u32) -> u32 {
    let mut inv = 1u32;
    for _ in 0..5 {
        inv = inv.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(inv)));
    }
    inv.wrapping_neg()
}

/// Whether `a` is at least `b`, both `n` words little-endian.
fn at_least(a: &[u32], b: &[u32]) -> bool {
    for (x, y) in a.iter().zip(b.iter()).rev() {
        if x != y {
            return x > y;
        }
    }
    true
}

/// `a -= b`, wrapping, both `n` words little-endian.
fn sub_assign(a: &mut [u32], b: &[u32]) {
    let mut borrow = 0u64;
    for (x, y) in a.iter_mut().zip(b.iter()) {
        let diff = u64::from(*x)
            .wrapping_sub(u64::from(*y))
            .wrapping_sub(borrow);
        *x = diff as u32;
        borrow = (diff >> 63) & 1;
    }
}

/// `R^2 mod m` with `R = 2^(32 n)`, division-free: `r` as IDF `calculate_rinv` computes it.
#[cfg(test)]
fn r_squared(m: &[u32]) -> Vec<u32> {
    let n = m.len();
    let mut r = vec![0u32; n];
    r[0] = 1;
    for _ in 0..(64 * n) {
        let mut carry = 0u32;
        for word in r.iter_mut() {
            let next = (*word << 1) | carry;
            carry = *word >> 31;
            *word = next;
        }
        // 2r < 2m, so at most one subtraction; a carry out of the top wraps to the same 2r - m.
        if carry != 0 || at_least(&r, m) {
            sub_assign(&mut r, m);
        }
    }
    r
}

/// Montgomery product `a b R^-1 mod m` (REDC over the full product), `a` and `b` below `m`.
fn mont_mul(a: &[u32], b: &[u32], m: &[u32], n_prime: u32) -> Vec<u32> {
    let n = m.len();
    let mut t = vec![0u32; 2 * n + 1];
    for (i, x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, y) in b.iter().enumerate() {
            let sum = u64::from(t[i + j]) + u64::from(*x) * u64::from(*y) + carry;
            t[i + j] = sum as u32;
            carry = sum >> 32;
        }
        let mut k = i + n;
        while carry != 0 {
            let sum = u64::from(t[k]) + carry;
            t[k] = sum as u32;
            carry = sum >> 32;
            k += 1;
        }
    }
    for i in 0..n {
        let u = t[i].wrapping_mul(n_prime);
        let mut carry = 0u64;
        for (j, y) in m.iter().enumerate() {
            let sum = u64::from(t[i + j]) + u64::from(u) * u64::from(*y) + carry;
            t[i + j] = sum as u32;
            carry = sum >> 32;
        }
        let mut k = i + n;
        while carry != 0 {
            let sum = u64::from(t[k]) + carry;
            t[k] = sum as u32;
            carry = sum >> 32;
            k += 1;
        }
    }
    let mut out = t[n..2 * n].to_vec();
    if t[2 * n] != 0 || at_least(&out, m) {
        sub_assign(&mut out, m);
    }
    out
}

/// `x * y mod m` for an odd `m` from the caller's `rr` and `n_prime`: the Montgomery product of
/// `x` and `y`, then of that with `rr`. With a wrong `M_PRIME` this gives what silicon answers
/// (the capture's 1 for `M_PRIME = 0`); the other order gives 1 there too, so which one the
/// pipeline runs is not settled.
fn mod_mul(x: &[u32], y: &[u32], m: &[u32], rr: &[u32], n_prime: u32) -> Vec<u32> {
    let product = mont_mul(&reduced(x, m), &reduced(y, m), m, n_prime);
    mont_mul(&product, rr, m, n_prime)
}

/// `x^y mod m` for an odd `m`, square and multiply over the bits of `y` from the top. With a
/// wrong `M_PRIME` or `r` the result is UNVERIFIED.
fn mod_exp(x: &[u32], y: &[u32], m: &[u32], rr: &[u32], n_prime: u32) -> Vec<u32> {
    let n = m.len();
    let mut one = vec![0u32; n];
    one[0] = 1;
    let base = mont_mul(&reduced(x, m), rr, m, n_prime);
    let mut acc = mont_mul(&one, rr, m, n_prime);
    let mut seen = false;
    for bit in (0..32 * n).rev() {
        let set = y[bit / 32] >> (bit % 32) & 1 != 0;
        if !seen && !set {
            continue;
        }
        if seen {
            acc = mont_mul(&acc, &acc, m, n_prime);
        }
        seen = true;
        if set {
            acc = mont_mul(&acc, &base, m, n_prime);
        }
    }
    mont_mul(&acc, &one, m, n_prime)
}

/// `a mod m` when `a` is below `2 m`: an operand the caller left at or above the modulus is
/// brought under it once.
fn reduced(a: &[u32], m: &[u32]) -> Vec<u32> {
    let mut out = a.to_vec();
    if at_least(&out, m) {
        sub_assign(&mut out, m);
    }
    out
}

impl Peripheral for Rsa {
    const ID: PeriphId = super::id::RSA;
    const BASE: u32 = 0x6003_C000;
    const SIZE: u32 = 0x1000;

    /// Every reset scope clears RSA.
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
    /// boundary.
    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let stop = self.store(off, size, val, cx.now, cx.ledger, cx.sched);
        self.sync_irq(cx.irq);
        RegWrite {
            stop,
            wiring: Wiring::None,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        if tag == TAG_DONE {
            self.complete();
            self.sync_irq(cx.irq);
        }
        Wiring::None
    }

    /// `QUERY_INTERRUPT` is set by the completion event only; `QUERY_CLEAN` never changes.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match off & !3 {
            reg::QUERY_INTERRUPT => Stability::UntilNextEvent,
            reg::QUERY_CLEAN => Stability::Until(VTime(u64::MAX)),
            _ => Stability::Never,
        }
    }

    /// The class `specs/blocks/rsa.toml` gives the register at `off`, `U` for the rest. The block
    /// has no rows in `specs/c3-registers.csv`, so codegen renders its classes into
    /// [`crate::gen::classes::rsa`].
    fn fidelity(&self, off: u32) -> Fidelity {
        crate::r#gen::classes::rsa::class_at(off)
    }
}

pub type Model = Rsa;

#[cfg(test)]
mod tests {
    use super::*;

    struct Host {
        rsa: Rsa,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
        stopped: bool,
    }

    impl Host {
        fn new() -> Host {
            Host {
                rsa: Rsa::default(),
                sched: Scheduler::default(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
                stopped: false,
            }
        }

        fn read(&mut self, off: u32) -> u32 {
            self.rsa.load(off, Size::B4, self.now, &mut self.ledger)
        }

        fn write(&mut self, off: u32, val: u32) {
            self.stopped = self.rsa.store(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.ledger,
                &mut self.sched,
            );
        }

        fn put(&mut self, base: u32, words: &[u32]) {
            for (i, word) in words.iter().enumerate() {
                self.write(base + 4 * i as u32, *word);
            }
        }

        fn pump(&mut self) {
            if let Some(t) = self.sched.next_time() {
                self.now = self.now.max(t);
            }
            while let Some(key) = self.sched.pop_due(self.now) {
                assert_eq!(key.owner, Owner::Periph(Rsa::ID));
                self.rsa.complete();
            }
        }

        /// The sequence `mpi_hal.c` performs: wait for the memory, load operands, `r` and
        /// `M_PRIME`, start, wait for completion, read `Z`.
        fn operate(&mut self, start: u32, x: &[u32], y: &[u32], m: &[u32]) -> Vec<u32> {
            assert_eq!(self.read(reg::QUERY_CLEAN), 1, "the loop while 0 must exit");
            self.put(reg::M, m);
            self.put(reg::X, x);
            self.put(reg::Y, y);
            self.put(reg::Z, &r_squared(m));
            self.write(reg::M_PRIME, mont_n_prime(m[0]));
            self.write(reg::LENGTH, m.len() as u32 - 1);
            self.write(start, 1);
            assert_eq!(self.read(reg::QUERY_INTERRUPT), 0, "busy until completion");
            self.pump();
            assert_eq!(self.read(reg::QUERY_INTERRUPT), 1);
            (0..m.len())
                .map(|i| self.read(reg::Z + 4 * i as u32))
                .collect()
        }
    }

    /// Both loops run while the bit is 0, so a model that reads 0 hangs the guest.
    #[test]
    fn the_two_seed_poll_bits_never_read_zero_when_idle() {
        let mut h = Host::new();
        assert_eq!(h.read(reg::QUERY_CLEAN), 1);
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 1);
        use pemu_core::reset::{ResetCause, ResetKind};
        h.put(reg::M, &[0x1234_5678]);
        h.rsa
            .apply_reset(ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset"));
        assert_eq!(h.read(reg::M), 0, "the operand memory is cleared");
        assert_eq!(h.read(reg::QUERY_CLEAN), 1);
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 1);
        assert_eq!(h.rsa.fidelity(reg::QUERY_CLEAN), Fidelity::B);
        assert_eq!(h.rsa.fidelity(reg::QUERY_INTERRUPT), Fidelity::B);
        assert_eq!(h.rsa.fidelity(reg::M), Fidelity::A);
    }

    #[test]
    fn modular_exponentiation_matches_the_arithmetic_for_one_word_operands() {
        let mut h = Host::new();
        let m = 0xFFFF_FFFBu32;
        for (x, y) in [
            (0x1234_5678u32, 0x0001_0001u32),
            (2, 0),
            (2, 1),
            (0xFFFF_FFFA, 0xFFFF_FFFA),
            (0, 5),
            (7, 0xFFFF_FFFF),
        ] {
            let got = h.operate(reg::MODEXP_START, &[x], &[y], &[m]);
            assert_eq!(got, vec![pow_mod32(x, y, m)], "{x:#X}^{y:#X} mod {m:#X}");
        }
    }

    /// The `probe_campaign_timing` capture: 0x12345678 times 0x9ABCDEF1 modulo 2^32 - 5 with
    /// `r = 25` gives 0x6D660A7E with the right `M_PRIME` 0xCCCCCCCD and 1 with `M_PRIME = 0`.
    #[test]
    fn a_wrong_m_prime_changes_the_answer_as_on_the_device() {
        let mut h = Host::new();
        let run = |h: &mut Host, m_prime: u32| {
            h.put(reg::M, &[0xFFFF_FFFB]);
            h.put(reg::X, &[0x1234_5678]);
            h.put(reg::Y, &[0x9ABC_DEF1]);
            h.put(reg::Z, &[25]);
            h.write(reg::M_PRIME, m_prime);
            h.write(reg::LENGTH, 0);
            h.write(reg::MOD_MULT_START, 1);
            h.pump();
            h.read(reg::Z)
        };
        assert_eq!(
            mont_n_prime(0xFFFF_FFFB),
            0xCCCC_CCCD,
            "the probe's right M'"
        );
        assert_eq!(run(&mut h, 0xCCCC_CCCD), 0x6D66_0A7E, "want, as the device");
        assert_eq!(run(&mut h, 0), 1, "M' = 0 gives 1, as the device");
    }

    #[test]
    fn modular_multiplication_matches_the_arithmetic_for_one_word_operands() {
        let mut h = Host::new();
        let m = 0xFFFF_FFFBu32;
        for (x, y) in [
            (0x1234_5678u32, 0x9ABC_DEF0u32),
            (0, 7),
            (1, 1),
            (m - 1, m - 1),
        ] {
            let got = h.operate(reg::MOD_MULT_START, &[x], &[y], &[m]);
            let want = ((u64::from(x) * u64::from(y)) % u64::from(m)) as u32;
            assert_eq!(got, vec![want], "{x:#X} * {y:#X} mod {m:#X}");
        }
    }

    /// A four-word modulus with an RSA-shaped exponent exercises the multi-word Montgomery path.
    #[test]
    fn multi_word_operands_agree_with_a_reference_ladder() {
        let mut h = Host::new();
        let m = [0x8765_4321u32, 0x1234_5678, 0xDEAD_BEEF, 0xF000_0001];
        let x = [0x0000_0007u32, 0x1111_1111, 0x2222_2222, 0x0000_0003];
        let e = [0x0001_0001u32, 0, 0, 0];
        let got = h.operate(reg::MODEXP_START, &x, &e, &m);
        assert_eq!(got, reference_pow(&x, &e, &m));
        assert_eq!(got.len(), 4);
        let got = h.operate(reg::MOD_MULT_START, &x, &m.map(|w| w >> 1), &m);
        assert_eq!(got, reference_mul(&x, &m.map(|w| w >> 1), &m));
    }

    #[test]
    fn an_even_modulus_leaves_the_result_alone_and_a_started_operation_still_completes() {
        // An even modulus is UNVERIFIED, so the model changes nothing.
        let mut h = Host::new();
        h.put(reg::Z, &[0xAAAA_AAAA]);
        h.put(reg::M, &[0x1000_0000]);
        h.put(reg::X, &[3]);
        h.put(reg::Y, &[5]);
        h.write(reg::LENGTH, 0);
        h.write(reg::MODEXP_START, 1);
        h.pump();
        assert_eq!(h.read(reg::Z), 0xAAAA_AAAA);
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 1, "the guest still gets out");
    }

    /// The xorshift32 stream the known answers were computed from (Python integers).
    fn words(seed: u32, n: usize) -> Vec<u32> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x
            })
            .collect()
    }

    fn le_hex(words: &[u32]) -> String {
        words.iter().map(|w| format!("{w:08x}")).collect()
    }

    /// Known answers from Python integers over [`words`]: `mult` is `X * Y`, `modmult` is
    /// `X * Y mod M`, `modexp65537` is `X ^ 65537 mod M`, `modexpfull` is `X ^ Y mod M`, and
    /// `modexp_trunc20` is `X ^ (Y mod 2^21) mod M`.
    const KNOWN: [(&str, usize, &str); 10] = [
        ("mult", 1, "0a20888200000022"),
        ("mult", 2, "3cc3330cd33673e4b716fcb200c095c9"),
        (
            "mult",
            16,
            "97e7ff9e2c0921bad861e5e3b5db25a4dcb619f8f0d843149fca87ff896a9409\
             725de98f047caa2fe20a9321fae88abc777699af2e459a05c74ad3222719705d\
             32dce01ab673000e4bf5483b2d0920f6a987a6e4d7504e6a06f3b4ca3e850d9d\
             40f6e192a618dded3ad8e2eeca3fd873783934e789d415a1dce7dfef08be50a9",
        ),
        (
            "mult",
            48,
            "1b8eee38c9a5f2291062da03133efa24c46e759fe1bab684a21a76aea2aecce9\
             15d2ed7e6cf1281abbdacee934fcab13c41e9c7602ee7629d3ee1abbbb9d2e35\
             fe69bc827786221aae87a1621640b3b596d8cf7862f279344c109b358820b896\
             b70e9bf93bd53fe5aedf8097979a8efce394f2f53c32009b528dc91327f811e3\
             50d2775b7134b528f488bcd89e6a9e73d9be6b317d3c74710a4f02b7cf2c2fa3\
             f63fa5b0864645642e0c9324b5309f9fdc9d6efcdf9095e0b96217a870835861\
             b8389d3f6ff4fd6df33f1ab5e942e355df0b623918179ad5e33287522e67c2dc\
             06b924c6d60125a1e2952a08e4b15f7e22b15242b87fea8199604b1869883a90\
             53a014fe47082d3da04fa5937da5fa541c2eb0ff8a270ab85d47ad0a8ced3c57\
             255e93acc3cb48567ea3cfe5cffdf6a075f47e6dd9b094d07918f369234d8753\
             526131d7ce3dd3f531a8278c7e407301815c39727bed57a4552adef3cd72019e\
             84401222dcce4e75ddde3a87199b181a69bfcd6ad2de49543d76450811eed802",
        ),
        (
            "modmult",
            16,
            "b7b31441c2035f571b0823b3fd2776f3a24ff221e0e170e899914da8de69bfeb\
             41cf25fdc435c80bb58e1a8be6ae54c092873c493ba4bb9386128db429c50645",
        ),
        (
            "modexp65537",
            16,
            "8141e1eff579da853d02ee7a52894212705839398e7994848ce89032f2881eec\
             633377314af29f5caa9204c91cf2dbbd25b4eaffa37b3f3ce65470d9ccb9a472",
        ),
        (
            "modexpfull",
            16,
            "bbdc2ec90f00539a061805340724534c914ecdf08c9c95b8bc1dc582368ac027\
             627b77be1570a78be983fdcadb84817b84837ae330f7e65c4d65f6c1202c34e4",
        ),
        (
            "modexp_trunc20",
            16,
            "fd0e7bf0aafc1a34fa82bce3a4305120f05a987852dfd90d2e44f86d2f49df89\
             e2c922cd295aeb3f03f741322d202635ed34d9969855ea3f7c09ccfc58fe03bf",
        ),
        (
            "modmult",
            64,
            "a786b4036806a204c0fb9c7e523ae02b1fcc45a7f173be551a05b359cbc1daef\
             180d0c30380ca71be1093acd298aad803a0d5f6e3a29404e09b8c466291196d4\
             95c76f2a50732901cb4b55ca16d3ce832a5d2196ff790ce204fda3ebd2b9fa0c\
             7b037afb6ae7743af9813f5143926f96dc43b520195addabb63fa86a0fa8cff8\
             143f4bd1699de55817c15035c79563689b7250d4e30603227f208bb2a5e71ad0\
             db5a6ed5d320ff0a06f755bbbf1c538fb971b5f4ab15f5ddfb4b7ba1f3472623\
             90011ff060ef7e0996f6437c65a013665147febaec9659b2e7c2acfbe638d57f\
             a5626e98c4b6a822b11f10a1ed9e7439567e5d006ef7fc66e24c8ff1a497d9e4",
        ),
        (
            "modexp65537",
            64,
            "a35ce8daa702b7ee38b430fd229d986062b760ce8d353e2ad3226a95dc9e0f70\
             6bb1ef1da0f59f38dae011f9888ebb31b7447db77b985e60a3cc326df7011399\
             e6e880d765d79606dd2fd24babb14fef87a6c18468586405377cdfbc7bb1f3e2\
             0d2e9117eb0557e04440860acb98759a4bbf73b27141881bdf31f369861c1e23\
             6c54bf6a7151c306da277e3ac357ea5145084ed7b1396a5b9e49e169b27b4b6a\
             a96382e6ece106b3c968ab2a9e1be0461c45fbd5995b8ea735082036c8d8bed2\
             0e49c6798c0ca18d677938357659b9b646831feaaaa4ae5a300b0940c373e4f0\
             6153962f6bb77423a0bae76a3a6452b300284f66af0464aef3224bb068766379",
        ),
    ];

    fn modular_operands(n: usize) -> (Vec<u32>, Vec<u32>, Vec<u32>) {
        let (sx, sy, sm) = if n == 16 { (9, 10, 11) } else { (12, 13, 14) };
        let mut m = words(sm, n);
        m[0] |= 1;
        m[n - 1] |= 0x8000_0000;
        (words(sx, n), words(sy, n), m)
    }

    /// `Z = X * Y` through the register sequence of IDF `esp_mpi_mul_mpi_hw_op`.
    fn multiply(h: &mut Host, x: &[u32], y: &[u32]) -> Vec<u32> {
        let n = x.len();
        h.put(reg::X, x);
        h.put(reg::Z + 4 * n as u32, y);
        h.write(reg::LENGTH, 2 * n as u32 - 1);
        h.write(reg::CLEAR_INTERRUPT, 1);
        h.write(reg::MULT_START, 1);
        assert!(h.stopped, "MULT_START scheduled the completion");
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 0, "busy until the completion");
        h.pump();
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 1);
        (0..2 * n).map(|i| h.read(reg::Z + 4 * i as u32)).collect()
    }

    /// Several lengths against answers computed outside this model, with the exponentiation run
    /// as IDF `esp_mpi_exp_mpi_mod_hw_op` runs it: search at `bitlen(Y) - 1`, constant time off.
    #[test]
    fn known_answers_at_several_lengths() {
        for (kind, n, want) in KNOWN {
            let mut h = Host::new();
            let got = match kind {
                "mult" => {
                    let seeds = [(1, 2), (3, 4), (5, 6), (7, 8)];
                    let (sx, sy) = seeds[[1, 2, 16, 48]
                        .iter()
                        .position(|w| *w == n)
                        .expect("a mult length")];
                    multiply(&mut h, &words(sx, n), &words(sy, n))
                }
                _ => {
                    let (x, mut y, m) = modular_operands(n);
                    let start = match kind {
                        "modmult" => reg::MOD_MULT_START,
                        _ => reg::MODEXP_START,
                    };
                    let search_pos = match kind {
                        "modexp65537" => {
                            y = vec![0; n];
                            y[0] = 65537;
                            Some(16)
                        }
                        "modexpfull" => Some(32 * n as u32 - 1 - y[n - 1].leading_zeros()),
                        "modexp_trunc20" => Some(20),
                        _ => None,
                    };
                    if let Some(pos) = search_pos {
                        h.write(reg::CONSTANT_TIME, 0);
                        h.write(reg::SEARCH_ENABLE, 1);
                        h.write(reg::SEARCH_POS, pos);
                    }
                    h.operate(start, &x, &y, &m)
                }
            };
            assert_eq!(le_hex(&got), want.replace(' ', ""), "{kind} at {n} words");
        }
    }

    /// With the search option, a position below the top bit gives the truncated exponent's answer
    /// (TRM 20.3.4).
    #[test]
    fn the_search_option_ignores_the_exponent_bits_above_its_position() {
        let (x, y, m) = modular_operands(16);
        let full = KNOWN[6].2;
        let truncated = KNOWN[7].2;
        let mut h = Host::new();
        h.write(reg::SEARCH_POS, 20);
        assert_eq!(le_hex(&h.operate(reg::MODEXP_START, &x, &y, &m)), full);
        h.write(reg::SEARCH_ENABLE, 1);
        assert_eq!(le_hex(&h.operate(reg::MODEXP_START, &x, &y, &m)), truncated);
        h.write(reg::SEARCH_POS, 511);
        assert_eq!(le_hex(&h.operate(reg::MODEXP_START, &x, &y, &m)), full);
    }

    /// `(2^32n - 1)^2` at every length the multiplier takes (1 to 48 words).
    #[test]
    fn the_product_of_all_ones_has_its_closed_form_at_every_length() {
        let mut h = Host::new();
        for n in 1..=MAX_WORDS / 2 {
            let ones = vec![u32::MAX; n];
            let got = multiply(&mut h, &ones, &ones);
            let mut want = vec![0u32; 2 * n];
            want[0] = 1;
            want[n] = 0xFFFF_FFFE;
            for word in want.iter_mut().skip(n + 1) {
                *word = u32::MAX;
            }
            assert_eq!(got, want, "{n} words");
        }
    }

    /// IDF leaves the low half of `Z_MEM` and the tail of `X_MEM` holding old data ("we don't zero
    /// the bottom words"), and it must not reach the product.
    #[test]
    fn the_multiplication_reads_only_its_operand_words() {
        let mut h = Host::new();
        h.put(reg::Z, &[0xDEAD_BEEF, 0x1234_5678]);
        h.put(reg::X + 8, &[0xCAFE_F00D; 4]);
        let got = multiply(&mut h, &[3, 0], &[5, 0]);
        assert_eq!(got, vec![15, 0, 0, 0]);
        assert_eq!(
            h.read(reg::X + 8),
            0xCAFE_F00D,
            "X_MEM beyond n is left alone"
        );
    }

    /// The interrupt is independent of the idle bit the HAL polls after its ISR has cleared it.
    #[test]
    fn the_completion_interrupt_latches_until_clear_interrupt() {
        let mut h = Host::new();
        assert_eq!(h.read(reg::INT_ENA), 1, "enabled at reset");
        h.write(reg::CLEAR_INTERRUPT, 1);
        let _ = multiply(&mut h, &[2], &[3]);
        assert!(h.rsa.irq_level(), "the completion raised it");
        h.write(reg::CLEAR_INTERRUPT, 0);
        assert!(
            h.rsa.irq_level(),
            "CLEAR_INTERRUPT written 0 clears nothing"
        );
        h.write(reg::CLEAR_INTERRUPT, 1);
        assert!(!h.rsa.irq_level());
        assert_eq!(
            h.read(reg::QUERY_INTERRUPT),
            1,
            "idle does not follow the latch"
        );
        assert_eq!(h.read(reg::CLEAR_INTERRUPT), 0, "write-only");

        for start in [reg::MODEXP_START, reg::MOD_MULT_START] {
            h.write(reg::CLEAR_INTERRUPT, 1);
            let _ = h.operate(start, &[3], &[5], &[0xFFFF_FFFB]);
            assert!(h.rsa.irq_level(), "{start:#X} raised it");
        }
        h.write(reg::INT_ENA, 0);
        assert!(!h.rsa.irq_level(), "INT_ENA gates the level");
        h.write(reg::INT_ENA, 1);
        assert!(h.rsa.irq_level(), "the latch survived the enable toggle");
    }

    #[test]
    fn the_registers_reset_to_the_trm_values_and_keep_their_widths() {
        let mut h = Host::new();
        assert_eq!(h.read(reg::CONSTANT_TIME), 1);
        assert_eq!(h.read(reg::SEARCH_ENABLE), 0);
        assert_eq!(h.read(reg::INT_ENA), 1);
        assert_eq!(h.read(reg::DATE), DATE_RESET);
        for (off, mask) in [
            (reg::LENGTH, 0x7F),
            (reg::CONSTANT_TIME, 1),
            (reg::SEARCH_ENABLE, 1),
            (reg::SEARCH_POS, 0xFFF),
            (reg::INT_ENA, 1),
            (reg::M_PRIME, u32::MAX),
        ] {
            h.write(off, u32::MAX);
            assert_eq!(h.read(off), mask, "offset {off:#X}");
        }
        for trigger in [reg::MODEXP_START, reg::MOD_MULT_START, reg::MULT_START] {
            h.write(trigger, 0);
            assert!(!h.stopped, "a trigger written 0 starts nothing");
            assert_eq!(h.read(trigger), 0, "a write-trigger reads 0");
        }
        use pemu_core::reset::{ResetCause, ResetKind};
        h.rsa
            .apply_reset(ResetKind::of(ResetCause(0x01)).expect("a documented power-on reset"));
        assert_eq!(h.read(reg::INT_ENA), 1);
        assert_eq!(h.read(reg::CONSTANT_TIME), 1);
        assert_eq!(h.read(reg::LENGTH), 0);
    }

    #[test]
    fn the_registers_carry_their_spec_classes() {
        let h = Host::new();
        // Class A: the rows the `probe_crypto` device capture proves.
        for off in [
            reg::M,
            reg::M + 0x17C,
            reg::Z,
            reg::Y,
            reg::X + 0x17C,
            reg::LENGTH,
            reg::MODEXP_START,
            reg::MOD_MULT_START,
            reg::MULT_START,
        ] {
            assert_eq!(h.rsa.fidelity(off), Fidelity::A, "offset {off:#X}");
        }
        for off in [
            reg::QUERY_CLEAN,
            reg::QUERY_INTERRUPT,
            reg::CLEAR_INTERRUPT,
            reg::SEARCH_ENABLE,
            reg::SEARCH_POS,
            reg::INT_ENA,
            reg::DATE,
        ] {
            assert_eq!(h.rsa.fidelity(off), Fidelity::B, "offset {off:#X}");
        }
        // Class A from the `probe_campaign_timing` capture: M_PRIME, and CONSTANT_TIME, which it
        // times set and clear.
        assert_eq!(h.rsa.fidelity(reg::M_PRIME), Fidelity::A);
        assert_eq!(h.rsa.fidelity(reg::CONSTANT_TIME), Fidelity::A);
        // The gaps between the 384-byte memory blocks are not memory.
        assert_eq!(h.rsa.fidelity(reg::M + 0x180), Fidelity::U);
    }

    #[test]
    fn the_completion_waits_for_the_profile_duration() {
        // One 64-word multiplication of 4.096 us is 1 ns at one word; a one-word exponentiation
        // with CONSTANT_TIME 1 takes 2 x 32 + 2 of them.
        let mut h = Host::new();
        h.rsa.set_op_ps(4_096_000);
        h.put(reg::M, &[0xFFFF_FFFB]);
        h.put(reg::X, &[3]);
        h.put(reg::Y, &[5]);
        h.put(reg::Z, &r_squared(&[0xFFFF_FFFB]));
        h.write(reg::M_PRIME, mont_n_prime(0xFFFF_FFFB));
        h.write(reg::LENGTH, 0);
        h.write(reg::MODEXP_START, 1);
        assert!(h.stopped, "MODEXP_START scheduled the completion");
        assert_eq!(h.sched.next_time(), Some(VTime(66_000)));
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 0);
        h.pump();
        assert_eq!(h.read(reg::QUERY_INTERRUPT), 1);
        assert_eq!(h.read(reg::Z), 243, "3^5 is below the modulus");
        h.write(reg::MODEXP_START, 0);
        assert!(!h.stopped, "nothing was scheduled, so nothing stops");
        assert_eq!(h.sched.next_time(), None);
    }

    /// The three 2048-bit exponentiations of `probe_campaign_timing` take the device's cycles
    /// within 0.02 % at the `device` profile's `rsa_op_ps`.
    #[test]
    fn a_2048_bit_exponentiation_takes_the_device_time() {
        const DEVICE_RSA_OP_PS: u64 = 53_644_000;
        let cases: [(&str, bool, bool, u64); 3] = [
            ("sparse_ct1", true, false, 35_173_669),
            ("sparse_ct0", false, false, 17_597_366),
            ("dense_ct0", false, true, 35_156_148),
        ];
        for (name, ct, dense, device_cycles) in cases {
            let mut h = Host::new();
            h.rsa.set_op_ps(DEVICE_RSA_OP_PS);
            let mut y = [0u32; 64];
            if dense {
                y = [u32::MAX; 64];
            } else {
                y[0] = 1;
                y[63] = 0x8000_0000;
            }
            h.put(reg::M, &[u32::MAX; 64]);
            h.put(reg::X, &[3; 64]);
            h.put(reg::Y, &y);
            h.write(reg::LENGTH, 63);
            h.write(reg::CONSTANT_TIME, u32::from(ct));
            h.write(reg::SEARCH_ENABLE, 0);
            h.write(reg::MODEXP_START, 1);
            let ps = h.sched.next_time().expect("the completion is scheduled").0;
            let cycles = ps as f64 / 6250.0;
            let err = (cycles - device_cycles as f64) / device_cycles as f64;
            assert!(
                err.abs() < 0.0002,
                "{name}: {cycles:.0} cycles against the device's {device_cycles} ({err:+.5})"
            );
        }
    }

    /// `CONSTANT_TIME` 0 charges each zero bit skipped above the top set bit at [`SKIP_BIT_PS`].
    #[test]
    fn the_acceleration_options_change_the_count() {
        let time = |ct: u32, search: Option<u32>| {
            let mut h = Host::new();
            h.rsa.set_op_ps(4_096_000); // 1 ns per one-word multiplication
            h.put(reg::M, &[0xFFFF_FFFB]);
            h.put(reg::X, &[3]);
            h.put(reg::Y, &[0x0001_0001]); // 65537: t = 16, h = 2
            h.write(reg::LENGTH, 0);
            h.write(reg::CONSTANT_TIME, ct);
            h.write(reg::SEARCH_ENABLE, u32::from(search.is_some()));
            h.write(reg::SEARCH_POS, search.unwrap_or(0));
            h.write(reg::MODEXP_START, 1);
            h.sched.next_time().expect("scheduled").0
        };
        assert_eq!(
            time(1, None),
            66_000,
            "32 bits, square and multiply each, + 2"
        );
        assert_eq!(time(1, Some(16)), 36_000, "17 bits considered");
        assert_eq!(time(0, Some(16)), 19_000, "16 squares, 1 multiply, + 2");
        assert_eq!(
            time(0, None),
            19_000 + 15 * SKIP_BIT_PS,
            "and the 15 zero bits above bit 16 are skipped at a cost"
        );
        let mut h = Host::new();
        h.put(reg::M, &[0xFFFF_FFFB]);
        h.write(reg::LENGTH, 0);
        h.write(reg::CONSTANT_TIME, 0);
        h.write(reg::MODEXP_START, 1);
        assert_eq!(h.sched.next_time(), Some(VTime(0)));
    }

    #[test]
    fn the_operand_windows_store_and_read_back_word_for_word() {
        let mut h = Host::new();
        for (i, base) in [reg::M, reg::Z, reg::Y, reg::X].iter().enumerate() {
            h.write(base + 4, 0x1000_0000 + i as u32);
        }
        for (i, base) in [reg::M, reg::Z, reg::Y, reg::X].iter().enumerate() {
            assert_eq!(h.read(base + 4), 0x1000_0000 + i as u32);
        }
        assert_eq!(h.read(reg::MEM_END - 4), 0, "the last word of X");
        let stopped = h.rsa.store(
            reg::M + 1,
            Size::B1,
            0xAB,
            h.now,
            &mut h.ledger,
            &mut h.sched,
        );
        assert!(!stopped, "a data-register write schedules nothing");
        assert_eq!(h.read(reg::M), 0x0000_AB00);
        assert_eq!(h.rsa.load(reg::M + 1, Size::B1, h.now, &mut h.ledger), 0xAB);
    }

    /// `x^y mod m` in u64 arithmetic, for the one-word cases.
    fn pow_mod32(x: u32, y: u32, m: u32) -> u32 {
        let (m64, mut base, mut acc, mut exp) =
            (u64::from(m), u64::from(x) % u64::from(m), 1u64, y);
        while exp > 0 {
            if exp & 1 == 1 {
                acc = acc * base % m64;
            }
            base = base * base % m64;
            exp >>= 1;
        }
        acc as u32
    }

    /// `x * y mod m` by shift and add, independent of the Montgomery path under test.
    fn reference_mul(x: &[u32], y: &[u32], m: &[u32]) -> Vec<u32> {
        let n = m.len();
        let mut acc = vec![0u32; n];
        let mut addend = reduced(x, m);
        for bit in 0..32 * n {
            if y[bit / 32] >> (bit % 32) & 1 != 0 {
                add_mod(&mut acc, &addend, m);
            }
            let doubled = addend.clone();
            add_mod(&mut addend, &doubled, m);
        }
        acc
    }

    /// `x^y mod m` by square and multiply over [`reference_mul`].
    fn reference_pow(x: &[u32], y: &[u32], m: &[u32]) -> Vec<u32> {
        let n = m.len();
        let mut acc = vec![0u32; n];
        acc[0] = 1;
        let mut base = reduced(x, m);
        for bit in 0..32 * n {
            if y[bit / 32] >> (bit % 32) & 1 != 0 {
                acc = reference_mul(&acc, &base, m);
            }
            base = reference_mul(&base, &base, m);
        }
        acc
    }

    /// `a = (a + b) mod m`, all below `m`.
    fn add_mod(a: &mut [u32], b: &[u32], m: &[u32]) {
        let mut carry = 0u64;
        for (x, y) in a.iter_mut().zip(b.iter()) {
            let sum = u64::from(*x) + u64::from(*y) + carry;
            *x = sum as u32;
            carry = sum >> 32;
        }
        if carry != 0 || at_least(a, m) {
            sub_assign(a, m);
        }
    }
}
