//! Machine-mode CSRs of the ESP32-C3 hart (ESP32-C3 TRM chapter 1, "1.4.1 Register Summary").
//!
//! [`Csr::access`] applies a [`CsrOp`] to one CSR number and returns the value the Zicsr
//! instruction reads, or the illegal-instruction [`Trap`] to take precisely at that instruction.
//!
//! - CSR 0x000 is a plain register (the ROM writes it in `_init`); an `mstatus` alias is
//!   UNVERIFIED and not modeled.
//! - `mie` and `mip` do not exist, the C3 INTC being memory-mapped (UNVERIFIED), so they trap, as
//!   do the unmodeled debug-mode CSRs.
//! - 0x7E0-0x7E2 and their user aliases 0x800-0x802 reach `Bus::csr_custom` with the instruction
//!   position, so the SoC derives the cycle counter from its clock; the `Csr` store keeps `mpcer`
//!   and `mpcmr` as architectural state (UNVERIFIED contract).
//! - Unknown CSRs go to `Bus::csr_custom` (the dedicated GPIO CSRs 0x803-0x805); with `strict`, a
//!   number outside the RISC-V custom ranges traps first.
//! - Machine mode only: CSR number bits 9:8 are unchecked.

use crate::bus::Bus;
use crate::pmp::{
    CSR_PMPADDR0, CSR_PMPCFG0, PMP_ENTRIES, PMPCFG_CSRS, read_pmpaddr, read_pmpcfg, write_pmpaddr,
    write_pmpcfg,
};
use crate::trap::{EXC_ILLEGAL_INSN, Trap};

pub const CSR_USTATUS: u16 = 0x000;
pub const CSR_MSTATUS: u16 = 0x300;
pub const CSR_MISA: u16 = 0x301;
pub const CSR_MIE: u16 = 0x304;
pub const CSR_MTVEC: u16 = 0x305;
pub const CSR_MSCRATCH: u16 = 0x340;
pub const CSR_MEPC: u16 = 0x341;
pub const CSR_MCAUSE: u16 = 0x342;
pub const CSR_MTVAL: u16 = 0x343;
pub const CSR_MIP: u16 = 0x344;
pub const CSR_TSELECT: u16 = 0x7A0;
pub const CSR_TDATA1: u16 = 0x7A1;
pub const CSR_TDATA2: u16 = 0x7A2;
pub const CSR_TCONTROL: u16 = 0x7A5;
/// Debug-mode CSRs `dcsr` to `dscratch1`: not modeled, they trap.
pub const CSR_DCSR: u16 = 0x7B0;
pub const CSR_DSCRATCH1: u16 = 0x7B3;
/// Performance counter event, mode and value (TRM Registers 1.12-1.14).
pub const CSR_MPCER: u16 = 0x7E0;
pub const CSR_MPCMR: u16 = 0x7E1;
pub const CSR_MPCCR: u16 = 0x7E2;
/// User aliases of the counter CSRs (UNVERIFIED aliasing for 0x800 and 0x801).
pub const CSR_UPCER: u16 = 0x800;
pub const CSR_UPCMR: u16 = 0x801;
pub const CSR_UPCCR: u16 = 0x802;
/// `cpu_gpio_in`: read-only, so the write trap is the CPU's; its value is the SoC's.
pub const CSR_CPU_GPIO_IN: u16 = 0x804;
/// Identification CSRs (TRM Registers 1.1-1.4).
pub const CSR_MVENDORID: u16 = 0xF11;
pub const CSR_MARCHID: u16 = 0xF12;
pub const CSR_MIMPID: u16 = 0xF13;
pub const CSR_MHARTID: u16 = 0xF14;

/// `mstatus` fields (IDF `riscv/encoding.h:35-49`).
pub const MSTATUS_MIE: u32 = 1 << 3;
pub const MSTATUS_MPIE: u32 = 1 << 7;
pub const MSTATUS_MPP: u32 = 3 << 11;
pub const MSTATUS_TW: u32 = 1 << 21;
/// Writable `mstatus` bits: bit 0, MIE, MPIE, MPP and TW. The TRM (Register 1.5) shows bit 0 as
/// reserved, but ROM `_init` writes 9 and C3 panic dumps show `MSTATUS 0x00001881`; bit 0 on
/// silicon is UNVERIFIED.
pub const MSTATUS_WRITE_MASK: u32 = 0x0020_1889;
/// `tcontrol` fields (IDF `riscv/csr.h:161-162`).
pub const TCONTROL_MTE: u32 = 1 << 3;
pub const TCONTROL_MPTE: u32 = 1 << 7;
/// Stored `tcontrol` bits (others UNVERIFIED).
pub const TCONTROL_WRITE_MASK: u32 = TCONTROL_MTE | TCONTROL_MPTE;
/// MODE is hardwired to 1 (TRM Register 1.7).
pub const MTVEC_BASE_MASK: u32 = 0xFFFF_FF00;
pub const MPCER_WRITE_MASK: u32 = 0x7FF;
/// Counts clock cycles, not during WFI.
pub const MPCER_CYCLE: u32 = 1 << 0;
/// `mpcmr` fields (TRM Register 1.13).
pub const MPCMR_COUNT_EN: u32 = 1 << 0;
pub const MPCMR_COUNT_SAT: u32 = 1 << 1;
pub const MPCMR_WRITE_MASK: u32 = MPCMR_COUNT_EN | MPCMR_COUNT_SAT;
/// MXL 1, U, M, I and C (TRM Register 1.6).
pub const MISA_VALUE: u32 = 0x4010_1104;
pub const MVENDORID_VALUE: u32 = 0x0000_0612;
pub const MARCHID_VALUE: u32 = 0x8000_0001;
pub const MIMPID_VALUE: u32 = 0x0000_0001;
/// Trigger slots in `Csr`: one per hart trigger, so every index tselect accepts has its own.
pub const TRIGGERS: usize = HART_TRIGGERS as usize;

/// Triggers tselect accepts; a larger write is ignored (UNVERIFIED WARL). IDF
/// `soc/esp32c3/include/soc/soc_caps.h:137-138` has 8 breakpoints and 8 watchpoints.
pub const HART_TRIGGERS: u32 = 8;

pub struct Csr {
    pub mstatus: u32,
    pub mtvec: u32,
    pub mepc: u32,
    pub mcause: u32,
    pub mtval: u32,
    pub mscratch: u32,
    pub pmpcfg: [u8; 16],
    pub pmpaddr: [u32; 16],
    pub tselect: u32,
    pub tdata1: [u32; TRIGGERS],
    pub tdata2: [u32; TRIGGERS],
    pub tcontrol: u32,
    pub mpcer: u32,
    pub mpcmr: u32,
    pub csr000: u32,
}

/// What [`Csr::access`] needs besides the CSR state.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CsrCx {
    /// Clock position of the accessing instruction (`Hart::pos`; `Hart::insns` under the `fast`
    /// profile), so the SoC derives clock-based values there.
    pub insns: u64,
    /// Raw instruction bits, the `mtval` of an illegal access.
    pub insn: u32,
    /// `EngineCfg::strict_csr`: unknown non-custom CSRs trap without reaching the bus.
    pub strict: bool,
}

impl Default for Csr {
    fn default() -> Self {
        Self::new()
    }
}

impl Csr {
    /// Reset state: TRM chapter 1 reset values, everything else 0 (`mstatus` 0 is UNVERIFIED).
    pub const fn new() -> Csr {
        Csr {
            mstatus: 0,
            mtvec: 1,
            mepc: 0,
            mcause: 0,
            mtval: 0,
            mscratch: 0,
            pmpcfg: [0; PMP_ENTRIES],
            pmpaddr: [0; PMP_ENTRIES],
            tselect: 0,
            tdata1: [0; TRIGGERS],
            tdata2: [0; TRIGGERS],
            tcontrol: 0,
            mpcer: 0,
            mpcmr: MPCMR_WRITE_MASK,
            csr000: 0,
        }
    }

    pub fn mstatus_mie(&self) -> bool {
        self.mstatus & MSTATUS_MIE != 0
    }

    /// Applies `op` to CSR `csr` and returns the value before it, which the instruction writes to
    /// `rd`. On `Err` nothing changed.
    pub fn access<B: Bus>(
        &mut self,
        bus: &mut B,
        cx: CsrCx,
        csr: u16,
        op: CsrOp,
    ) -> Result<u32, Trap> {
        let illegal = Trap::illegal_instruction(cx.insn);
        if op.writes() && is_read_only(csr) {
            return Err(illegal);
        }
        let value = match csr {
            CSR_USTATUS => store(&mut self.csr000, op, |v| v),
            CSR_MSTATUS => store(&mut self.mstatus, op, legal_mstatus),
            CSR_MISA => MISA_VALUE,
            CSR_MTVEC => store(&mut self.mtvec, op, legal_mtvec),
            CSR_MSCRATCH => store(&mut self.mscratch, op, |v| v),
            CSR_MEPC => store(&mut self.mepc, op, |v| v & !1),
            CSR_MCAUSE => store(&mut self.mcause, op, |v| v),
            CSR_MTVAL => store(&mut self.mtval, op, |v| v),
            CSR_MVENDORID => MVENDORID_VALUE,
            CSR_MARCHID => MARCHID_VALUE,
            CSR_MIMPID => MIMPID_VALUE,
            CSR_MHARTID => 0,
            _ if pmpcfg_index(csr).is_some() => self.pmpcfg_access(bus, csr, op),
            _ if pmpaddr_index(csr).is_some() => self.pmpaddr_access(bus, csr, op),
            CSR_TSELECT => self.tselect_access(op),
            CSR_TDATA1 | CSR_TDATA2 => self.tdata_access(csr, op),
            CSR_TCONTROL => store(&mut self.tcontrol, op, |v| v & TCONTROL_WRITE_MASK),
            CSR_MPCER | CSR_UPCER | CSR_MPCMR | CSR_UPCMR => {
                self.counter_ctrl_access(bus, cx, csr, op)?
            }
            CSR_MPCCR | CSR_UPCCR => custom(bus, cx, csr, op)?,
            CSR_MIE | CSR_MIP | CSR_DCSR..=CSR_DSCRATCH1 => return Err(illegal),
            _ if cx.strict && !is_custom(csr) => return Err(illegal),
            _ => custom(bus, cx, csr, op)?,
        };
        Ok(value)
    }

    pub fn read<B: Bus>(&mut self, bus: &mut B, cx: CsrCx, csr: u16) -> Result<u32, Trap> {
        self.access(bus, cx, csr, CsrOp::Read)
    }

    pub fn write<B: Bus>(
        &mut self,
        bus: &mut B,
        cx: CsrCx,
        csr: u16,
        value: u32,
    ) -> Result<(), Trap> {
        self.access(bus, cx, csr, CsrOp::Write(value)).map(|_| ())
    }

    fn pmpcfg_access<B: Bus>(&mut self, bus: &mut B, csr: u16, op: CsrOp) -> u32 {
        let n = pmpcfg_index(csr).expect("caller checked the pmpcfg range");
        let old = read_pmpcfg(&self.pmpcfg, n);
        if let Some(new) = op.new_value(old)
            && write_pmpcfg(&mut self.pmpcfg, n, new)
        {
            bus.pmp_changed(self);
        }
        old
    }

    fn pmpaddr_access<B: Bus>(&mut self, bus: &mut B, csr: u16, op: CsrOp) -> u32 {
        let i = pmpaddr_index(csr).expect("caller checked the pmpaddr range");
        let old = read_pmpaddr(&self.pmpaddr, i);
        if let Some(new) = op.new_value(old)
            && write_pmpaddr(&self.pmpcfg, &mut self.pmpaddr, i, new)
        {
            bus.pmp_changed(self);
        }
        old
    }

    fn tselect_access(&mut self, op: CsrOp) -> u32 {
        let old = self.tselect;
        if let Some(new) = op.new_value(old)
            && new < HART_TRIGGERS
        {
            self.tselect = new;
        }
        old
    }

    /// The `None` arm is unreachable (tselect stays below [`TRIGGERS`]); it reads 0 rather than
    /// aliasing another trigger.
    fn tdata_access(&mut self, csr: u16, op: CsrOp) -> u32 {
        let slots = if csr == CSR_TDATA1 {
            &mut self.tdata1
        } else {
            &mut self.tdata2
        };
        match slots.get_mut(self.tselect as usize) {
            Some(slot) => store(slot, op, |v| v),
            None => 0,
        }
    }

    /// Reports the access to the clock through `Bus::csr_custom` before the store changes.
    fn counter_ctrl_access<B: Bus>(
        &mut self,
        bus: &mut B,
        cx: CsrCx,
        csr: u16,
        op: CsrOp,
    ) -> Result<u32, Trap> {
        let (slot, mask) = match csr {
            CSR_MPCER | CSR_UPCER => (&mut self.mpcer, MPCER_WRITE_MASK),
            _ => (&mut self.mpcmr, MPCMR_WRITE_MASK),
        };
        let old = *slot;
        let new = op.new_value(old).map(|v| v & mask);
        let forwarded = new.map_or(CsrOp::Read, CsrOp::Write);
        custom(bus, cx, csr, forwarded)?;
        if let Some(new) = new {
            *slot = new;
        }
        Ok(old)
    }
}

fn store(slot: &mut u32, op: CsrOp, legalize: impl Fn(u32) -> u32) -> u32 {
    let old = *slot;
    if let Some(new) = op.new_value(old) {
        *slot = legalize(new);
    }
    old
}

/// A bus trap keeps its cause; an illegal-instruction one gets this instruction's bits in mtval.
fn custom<B: Bus>(bus: &mut B, cx: CsrCx, csr: u16, op: CsrOp) -> Result<u32, Trap> {
    bus.csr_custom(csr, op, cx.insns)
        .map(|(value, _effect)| value)
        .map_err(|trap| {
            if trap.cause == EXC_ILLEGAL_INSN {
                Trap::illegal_instruction(cx.insn)
            } else {
                trap
            }
        })
}

/// Bit 11 sets both MPP bits (TRM Register 1.5: "Only lower bit is writable").
pub const fn legal_mstatus(value: u32) -> u32 {
    let mpp = if value & (1 << 11) != 0 {
        MSTATUS_MPP
    } else {
        0
    };
    (value & MSTATUS_WRITE_MASK & !MSTATUS_MPP) | mpp
}

pub const fn legal_mtvec(value: u32) -> u32 {
    (value & MTVEC_BASE_MASK) | 1
}

/// Bits 11:10 are 0b11 (RISC-V Privileged Specification), or [`CSR_CPU_GPIO_IN`], which the TRM
/// "1.4.1" table marks RO inside a writable range.
pub const fn is_read_only(csr: u16) -> bool {
    (csr >> 10) & 0b11 == 0b11 || csr == CSR_CPU_GPIO_IN
}

/// A custom range of the RISC-V Privileged Specification table "Allocation of RISC-V CSR
/// addresses".
pub const fn is_custom(csr: u16) -> bool {
    matches!(
        csr,
        0x800..=0x8FF
            | 0xCC0..=0xCFF
            | 0x5C0..=0x5FF
            | 0x9C0..=0x9FF
            | 0xDC0..=0xDFF
            | 0x6C0..=0x6FF
            | 0xAC0..=0xAFF
            | 0xEC0..=0xEFF
            | 0x7C0..=0x7FF
            | 0xBC0..=0xBFF
            | 0xFC0..=0xFFF
    )
}

fn pmpcfg_index(csr: u16) -> Option<usize> {
    let n = csr.wrapping_sub(CSR_PMPCFG0) as usize;
    (n < PMPCFG_CSRS).then_some(n)
}

fn pmpaddr_index(csr: u16) -> Option<usize> {
    let i = csr.wrapping_sub(CSR_PMPADDR0) as usize;
    (i < PMP_ENTRIES).then_some(i)
}

/// Operation a Zicsr instruction applies to a CSR. `csrrs`/`csrrc` with `rs1 = x0` or `uimm = 0`
/// decode to `Read`, since Zicsr says they do not write.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CsrOp {
    Read,
    Write(u32),
    Set(u32),
    Clear(u32),
}

impl CsrOp {
    /// `Set(0)` and `Clear(0)` still count as writes.
    pub const fn writes(self) -> bool {
        !matches!(self, CsrOp::Read)
    }

    pub const fn new_value(self, old: u32) -> Option<u32> {
        match self {
            CsrOp::Read => None,
            CsrOp::Write(v) => Some(v),
            CsrOp::Set(bits) => Some(old | bits),
            CsrOp::Clear(bits) => Some(old & !bits),
        }
    }
}

/// Side effect of a custom CSR access; ignored, since a CSR instruction already ends its block.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CsrEffect {
    None,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn csr_with_mstatus(mstatus: u32) -> Csr {
        Csr {
            mstatus,
            ..Csr::new()
        }
    }

    #[test]
    fn mstatus_mie_reads_bit_3() {
        assert!(!csr_with_mstatus(0).mstatus_mie());
        assert!(csr_with_mstatus(0x8).mstatus_mie());
        assert!(!csr_with_mstatus(0x0020_1881).mstatus_mie());
        assert!(csr_with_mstatus(0x0020_1889).mstatus_mie());
    }

    #[test]
    fn legal_mstatus_keeps_the_mask_and_ties_mpp_to_bit_11() {
        assert_eq!(legal_mstatus(u32::MAX), MSTATUS_WRITE_MASK);
        assert_eq!(legal_mstatus(0x9), 0x9);
        assert_eq!(legal_mstatus(1 << 11), MSTATUS_MPP);
        assert_eq!(legal_mstatus(1 << 12), 0);
        assert_eq!(legal_mstatus(0x0000_1881), 0x0000_1881);
        assert_eq!(legal_mstatus(MSTATUS_TW | 0x0000_6000), MSTATUS_TW);
    }

    #[test]
    fn legal_mtvec_forces_vectored_mode_and_a_256_byte_base() {
        assert_eq!(legal_mtvec(0x4038_0000), 0x4038_0001);
        assert_eq!(legal_mtvec(0x4000_1D01), 0x4000_1D01);
        assert_eq!(legal_mtvec(0x4038_00FE), 0x4038_0001);
        assert_eq!(legal_mtvec(0), 1);
    }

    #[test]
    fn read_only_and_custom_csr_ranges() {
        for csr in [
            CSR_MVENDORID,
            CSR_MHARTID,
            CSR_CPU_GPIO_IN,
            0xC00,
            0xFC0,
            0xFFF,
        ] {
            assert!(is_read_only(csr), "{csr:#x}");
        }
        // The dedicated GPIO CSRs around 0x804 stay writable (TRM "1.4.1": R/W).
        for csr in [
            CSR_USTATUS,
            CSR_MSTATUS,
            CSR_MPCCR,
            CSR_UPCCR,
            0x803,
            0x805,
            0xBFF,
        ] {
            assert!(!is_read_only(csr), "{csr:#x}");
        }
        for csr in [
            CSR_MPCCR, CSR_UPCER, 0x803, 0x8FF, 0x7C5, 0xBC0, 0xCC0, 0xFFF,
        ] {
            assert!(is_custom(csr), "{csr:#x}");
        }
        for csr in [
            CSR_USTATUS,
            CSR_MSTATUS,
            CSR_TSELECT,
            CSR_DCSR,
            0x7BF,
            0xC00,
            0xF14,
        ] {
            assert!(!is_custom(csr), "{csr:#x}");
        }
    }

    #[test]
    fn csr_op_new_value_follows_zicsr() {
        assert_eq!(CsrOp::Read.new_value(0xF0), None);
        assert_eq!(CsrOp::Write(5).new_value(0xF0), Some(5));
        assert_eq!(CsrOp::Set(0x0F).new_value(0xF0), Some(0xFF));
        assert_eq!(CsrOp::Clear(0x30).new_value(0xF0), Some(0xC0));
        assert!(!CsrOp::Read.writes());
        assert!(CsrOp::Set(0).writes());
        assert!(CsrOp::Clear(0).writes());
    }

    #[test]
    fn pmp_csr_numbers_map_to_indices() {
        assert_eq!(pmpcfg_index(0x3A0), Some(0));
        assert_eq!(pmpcfg_index(0x3A3), Some(3));
        assert_eq!(pmpcfg_index(0x3A4), None);
        assert_eq!(pmpcfg_index(0x39F), None);
        assert_eq!(pmpaddr_index(0x3B0), Some(0));
        assert_eq!(pmpaddr_index(0x3BF), Some(15));
        assert_eq!(pmpaddr_index(0x3C0), None);
        assert_eq!(pmpaddr_index(0x000), None);
    }

    #[test]
    fn reset_values_follow_the_trm() {
        let c = Csr::new();
        assert_eq!(c.mtvec, 1);
        assert_eq!(c.mpcmr, MPCMR_COUNT_EN | MPCMR_COUNT_SAT);
        assert_eq!(
            (c.mstatus, c.mepc, c.mcause, c.mtval, c.mscratch),
            (0, 0, 0, 0, 0)
        );
        assert_eq!((c.tselect, c.tcontrol, c.mpcer, c.csr000), (0, 0, 0, 0));
        assert_eq!(c.pmpcfg, [0; 16]);
        assert_eq!(c.pmpaddr, [0; 16]);
        assert_eq!(c.tdata1.len(), TRIGGERS);
    }
}
