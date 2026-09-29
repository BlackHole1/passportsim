//! The ESP32-C3 CSRs beyond the standard machine-mode set (ESP32-C3 TRM chapter 1), through
//! `Csr::access`: performance counters and their user aliases, CSR 0x000, PMP, vectored mtvec,
//! the mstatus write mask, triggers, identification and unknown CSRs. `ClockBus` stands in for the
//! SoC with a CPI 1 cycle counter; the tests pin down the CPU side: which CSR number, op and
//! instruction count reach `Bus::csr_custom`, and what state `Csr` keeps.

use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PageTable};
use pemu_rv32::csr::*;
use pemu_rv32::exec::Hart;
use pemu_rv32::pmp::{
    AccessKind, CSR_PMPADDR0, CSR_PMPCFG0, PMP_L, PMP_R, PMP_TOR, PMP_W, PMP_X, Pmp, Privilege,
};
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::{self, EXC_ILLEGAL_INSN, Trap};

/// Counts retired instructions while enabled; nothing retires in WFI (TRM Register 1.12).
#[derive(Debug, Default)]
struct CycleClock {
    base_cc: u64,
    base_insns: u64,
    pcer: u32,
    pcmr: u32,
}

impl CycleClock {
    fn enabled(&self) -> bool {
        self.pcer & MPCER_CYCLE != 0 && self.pcmr & MPCMR_COUNT_EN != 0
    }

    fn count(&self, insns: u64) -> u64 {
        if self.enabled() {
            self.base_cc + (insns - self.base_insns)
        } else {
            self.base_cc
        }
    }

    fn rebase(&mut self, insns: u64, count: u64) {
        self.base_cc = count;
        self.base_insns = insns;
    }
}

struct ClockBus {
    clock: CycleClock,
    /// CSR number, op, instruction count.
    calls: Vec<(u16, CsrOp, u64)>,
    pmp_changes: Vec<([u8; 16], [u32; 16])>,
    /// `csr_custom` refuses this CSR with `refuse_trap`.
    refuse: Option<u16>,
    refuse_trap: Trap,
}

impl ClockBus {
    fn new() -> Self {
        ClockBus {
            clock: CycleClock {
                pcmr: MPCMR_WRITE_MASK,
                ..CycleClock::default()
            },
            calls: Vec::new(),
            pmp_changes: Vec::new(),
            refuse: None,
            refuse_trap: REFUSED,
        }
    }
}

const CUSTOM_VALUE: u32 = 0x00C0_FFEE;

/// Neither field is one the CPU could invent, so a test sees whether the bus's trap survives.
const REFUSED: Trap = Trap {
    cause: 0x0BAD,
    tval: 0x1234_5678,
};

impl Bus for ClockBus {
    fn pages(&self) -> &PageTable {
        unreachable!("CSR tests touch no memory")
    }
    fn arena(&mut self) -> *mut u8 {
        unreachable!("CSR tests touch no memory")
    }
    fn load_slow(&mut self, _addr: u32, _size: u8, _hart: &HartView) -> Access<u32> {
        unreachable!("CSR tests touch no memory")
    }
    fn store_slow(&mut self, _addr: u32, _size: u8, _val: u32, _hart: &HartView) -> Access<()> {
        unreachable!("CSR tests touch no memory")
    }
    fn sp_monitor(&self) -> SpMonitor {
        unreachable!("CSR tests touch no memory")
    }
    fn fetch_code(&mut self, _vaddr: u32) -> Result<CodePage<'_>, Trap> {
        unreachable!("CSR tests touch no memory")
    }
    fn csr_custom(&mut self, csr: u16, op: CsrOp, insns: u64) -> Result<(u32, CsrEffect), Trap> {
        self.calls.push((csr, op, insns));
        if self.refuse == Some(csr) {
            return Err(self.refuse_trap);
        }
        let clock = &mut self.clock;
        match (csr, op) {
            (CSR_MPCER | CSR_UPCER, CsrOp::Write(v)) => {
                clock.rebase(insns, clock.count(insns));
                clock.pcer = v;
            }
            (CSR_MPCMR | CSR_UPCMR, CsrOp::Write(v)) => {
                clock.rebase(insns, clock.count(insns));
                clock.pcmr = v;
            }
            (CSR_MPCER | CSR_UPCER | CSR_MPCMR | CSR_UPCMR, _) => {}
            (CSR_MPCCR | CSR_UPCCR, _) => {
                let old = clock.count(insns) as u32;
                if let Some(new) = op.new_value(old) {
                    clock.rebase(insns, u64::from(new));
                }
                return Ok((old, CsrEffect::None));
            }
            _ => return Ok((CUSTOM_VALUE, CsrEffect::None)),
        }
        // The CPU ignores this value for 0x7E0/0x7E1 and their aliases.
        Ok((u32::MAX, CsrEffect::None))
    }
    fn wfi_wake(&mut self) -> bool {
        unreachable!("CSR tests do not wait")
    }
    fn pmp_changed(&mut self, csr: &Csr) {
        self.pmp_changes.push((csr.pmpcfg, csr.pmpaddr));
    }
}

/// `funct3` 1 csrrw, 2 csrrs, 3 csrrc, 5-7 immediate forms.
fn zicsr(csr: u16, funct3: u32, rd: u32, rs1: u32) -> u32 {
    (u32::from(csr) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x73
}

fn at(insns: u64, insn: u32) -> CsrCx {
    CsrCx {
        insns,
        insn,
        strict: false,
    }
}

/// `csrrw a0, csr, a1`.
fn rw(insns: u64, csr: u16) -> CsrCx {
    at(insns, zicsr(csr, 1, 10, 11))
}

fn hart(pc: u32) -> Hart {
    Hart {
        x: [0; 32],
        pc,
        csr: Csr::new(),
        wfi: false,
        insns: 0,
        stores: 0,
        spmon: SpMonitor::default(),
        extra: 0,
        pipe: Default::default(),
    }
}

#[test]
fn rom_enables_the_cycle_counter_through_the_user_aliases() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    // ROM `_init` writes 0x800 = 1 and 0x801 = 1; `ets_delay_us` then polls 0x802.
    csr.write(&mut bus, rw(10, CSR_UPCER), CSR_UPCER, 1)
        .unwrap();
    csr.write(&mut bus, rw(11, CSR_UPCMR), CSR_UPCMR, 1)
        .unwrap();
    assert_eq!((csr.mpcer, csr.mpcmr), (MPCER_CYCLE, MPCMR_COUNT_EN));

    let start = csr.read(&mut bus, rw(12, CSR_UPCCR), CSR_UPCCR).unwrap();
    let later = csr.read(&mut bus, rw(112, CSR_UPCCR), CSR_UPCCR).unwrap();
    assert_eq!(later - start, 100, "one cycle per retired instruction");
    assert_eq!(csr.read(&mut bus, rw(112, CSR_MPCCR), CSR_MPCCR), Ok(later));
    assert_eq!(csr.read(&mut bus, rw(113, CSR_MPCER), CSR_MPCER), Ok(1));
    assert_eq!(csr.read(&mut bus, rw(114, CSR_MPCMR), CSR_MPCMR), Ok(1));

    assert_eq!(
        bus.calls,
        [
            (CSR_UPCER, CsrOp::Write(1), 10),
            (CSR_UPCMR, CsrOp::Write(1), 11),
            (CSR_UPCCR, CsrOp::Read, 12),
            (CSR_UPCCR, CsrOp::Read, 112),
            (CSR_MPCCR, CsrOp::Read, 112),
            (CSR_MPCER, CsrOp::Read, 113),
            (CSR_MPCMR, CsrOp::Read, 114),
        ]
    );
}

#[test]
fn every_access_to_a_time_derived_csr_reaches_the_bus() {
    // The SoC tracks time-derived reads in `csr_custom`, so none may be served from the store.
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    let all = [
        CSR_MPCER, CSR_MPCMR, CSR_MPCCR, CSR_UPCER, CSR_UPCMR, CSR_UPCCR,
    ];
    for (i, &n) in all.iter().enumerate() {
        csr.read(&mut bus, rw(i as u64, n), n).unwrap();
    }
    let expected: Vec<_> = all
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, CsrOp::Read, i as u64))
        .collect();
    assert_eq!(bus.calls, expected);
}

#[test]
fn counter_control_csrs_mask_writes_and_share_storage_with_the_aliases() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    // mpcmr resets to COUNT_EN | COUNT_SAT (TRM Register 1.13), seen through both numbers.
    assert_eq!(csr.read(&mut bus, rw(0, CSR_MPCMR), CSR_MPCMR), Ok(0b11));
    assert_eq!(csr.read(&mut bus, rw(0, CSR_UPCMR), CSR_UPCMR), Ok(0b11));

    assert_eq!(
        csr.access(
            &mut bus,
            rw(1, CSR_MPCER),
            CSR_MPCER,
            CsrOp::Write(u32::MAX)
        ),
        Ok(0)
    );
    assert_eq!(
        csr.read(&mut bus, rw(2, CSR_UPCER), CSR_UPCER),
        Ok(MPCER_WRITE_MASK)
    );
    let clear = at(3, zicsr(CSR_UPCER, 3, 10, 11));
    assert_eq!(
        csr.access(&mut bus, clear, CSR_UPCER, CsrOp::Clear(0x7FE)),
        Ok(0x7FF)
    );
    assert_eq!(
        csr.read(&mut bus, rw(4, CSR_MPCER), CSR_MPCER),
        Ok(MPCER_CYCLE)
    );

    assert_eq!(
        csr.access(&mut bus, rw(5, CSR_UPCMR), CSR_UPCMR, CsrOp::Write(!0b11)),
        Ok(0b11)
    );
    assert_eq!(csr.mpcmr, 0);
    let set = at(6, zicsr(CSR_MPCMR, 2, 10, 11));
    assert_eq!(csr.access(&mut bus, set, CSR_MPCMR, CsrOp::Set(0)), Ok(0));

    // The bus sees the legalized value the store takes, not the instruction's operand.
    assert_eq!(bus.calls[2], (CSR_MPCER, CsrOp::Write(MPCER_WRITE_MASK), 1));
    assert_eq!(bus.calls[4], (CSR_UPCER, CsrOp::Write(MPCER_CYCLE), 3));
    assert_eq!(bus.calls[6], (CSR_UPCMR, CsrOp::Write(0), 5));
    assert_eq!(bus.calls[7], (CSR_MPCMR, CsrOp::Write(0), 6));
    assert_eq!((bus.clock.pcer, bus.clock.pcmr), (csr.mpcer, csr.mpcmr));
}

#[test]
fn cycle_counter_can_be_set_and_stops_while_disabled() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    csr.write(&mut bus, rw(0, CSR_MPCER), CSR_MPCER, MPCER_CYCLE)
        .unwrap();
    assert_eq!(csr.read(&mut bus, rw(50, CSR_MPCCR), CSR_MPCCR), Ok(50));

    // `esp_cpu_set_cycle_count` writes 0x7E2.
    let set = CsrOp::Write(1000);
    assert_eq!(
        csr.access(&mut bus, rw(60, CSR_MPCCR), CSR_MPCCR, set),
        Ok(60)
    );
    assert_eq!(bus.calls.last(), Some(&(CSR_MPCCR, set, 60)));
    assert_eq!(csr.read(&mut bus, rw(70, CSR_UPCCR), CSR_UPCCR), Ok(1010));

    let off = at(80, zicsr(CSR_MPCMR, 3, 0, 11));
    csr.access(&mut bus, off, CSR_MPCMR, CsrOp::Clear(MPCMR_COUNT_EN))
        .unwrap();
    assert_eq!(csr.read(&mut bus, rw(500, CSR_MPCCR), CSR_MPCCR), Ok(1020));
    let on = at(600, zicsr(CSR_UPCMR, 2, 0, 11));
    csr.access(&mut bus, on, CSR_UPCMR, CsrOp::Set(MPCMR_COUNT_EN))
        .unwrap();
    assert_eq!(csr.read(&mut bus, rw(610, CSR_UPCCR), CSR_UPCCR), Ok(1030));
}

#[test]
fn cycle_counter_pauses_in_wfi() {
    let mut bus = ClockBus::new();
    let mut h = hart(0x4200_0100);
    h.csr.mtvec = 0x4038_0001;
    h.csr.mstatus = MSTATUS_MIE;
    h.csr
        .write(&mut bus, rw(h.insns, CSR_UPCER), CSR_UPCER, 1)
        .unwrap();
    h.csr
        .write(&mut bus, rw(h.insns, CSR_UPCMR), CSR_UPCMR, 1)
        .unwrap();

    h.insns += 1000; // a busy loop
    let before = h
        .csr
        .read(&mut bus, rw(h.insns, CSR_MPCCR), CSR_MPCCR)
        .unwrap();
    assert_eq!(before, 1000);

    // Nothing retires while the hart waits (TRM Register 1.12), nor does the wake-up interrupt.
    h.insns += 1;
    h.wfi = true;
    let waiting = h.insns;
    trap::take_interrupt(&mut h, 7);
    assert!(!h.wfi);
    assert_eq!(h.insns, waiting);
    let after = h
        .csr
        .read(&mut bus, rw(h.insns, CSR_MPCCR), CSR_MPCCR)
        .unwrap();
    assert_eq!(after, before + 1, "only the wfi instruction itself counted");

    h.insns += 10; // the interrupt handler runs
    assert_eq!(
        h.csr.read(&mut bus, rw(h.insns, CSR_UPCCR), CSR_UPCCR),
        Ok(after + 10)
    );
    let insns: Vec<u64> = bus.calls.iter().map(|&(_, _, n)| n).collect();
    assert_eq!(insns, [0, 0, 1000, 1001, 1011]);
}

#[test]
fn a_bus_trap_on_a_counter_csr_leaves_the_store_unchanged() {
    let mut bus = ClockBus::new();
    bus.refuse = Some(CSR_UPCMR);
    let mut csr = Csr::new();
    let cx = rw(9, CSR_UPCMR);
    assert_eq!(csr.write(&mut bus, cx, CSR_UPCMR, 0), Err(REFUSED));
    assert_eq!(csr.mpcmr, MPCMR_WRITE_MASK);
}

#[test]
fn csr_000_is_a_plain_register() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    // ROM `_init`: `csrw 0x0, 0`, later `csrw 0x0, 1`; neither may trap.
    csr.write(&mut bus, rw(0, CSR_USTATUS), CSR_USTATUS, 0)
        .unwrap();
    csr.write(&mut bus, rw(1, CSR_USTATUS), CSR_USTATUS, 1)
        .unwrap();
    assert_eq!(csr.read(&mut bus, rw(2, CSR_USTATUS), CSR_USTATUS), Ok(1));
    let all = CsrOp::Write(u32::MAX);
    assert_eq!(
        csr.access(&mut bus, rw(3, CSR_USTATUS), CSR_USTATUS, all),
        Ok(1)
    );
    assert_eq!(csr.csr000, u32::MAX, "every bit is stored");
    let clear = CsrOp::Clear(0xF0);
    assert_eq!(
        csr.access(&mut bus, rw(4, CSR_USTATUS), CSR_USTATUS, clear),
        Ok(u32::MAX)
    );
    assert_eq!(
        csr.read(&mut bus, rw(5, CSR_USTATUS), CSR_USTATUS),
        Ok(0xFFFF_FF0F)
    );
    assert_eq!(csr.mstatus, 0, "not aliased onto mstatus");
    assert!(bus.calls.is_empty());
}

#[test]
fn mstatus_keeps_only_its_write_mask() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    // ROM `_init` writes 0 and then 9, MIE plus bit 0.
    csr.write(&mut bus, rw(0, CSR_MSTATUS), CSR_MSTATUS, 0)
        .unwrap();
    csr.write(&mut bus, rw(1, CSR_MSTATUS), CSR_MSTATUS, 9)
        .unwrap();
    assert_eq!(csr.read(&mut bus, rw(2, CSR_MSTATUS), CSR_MSTATUS), Ok(9));
    assert!(csr.mstatus_mie());

    csr.write(&mut bus, rw(3, CSR_MSTATUS), CSR_MSTATUS, u32::MAX)
        .unwrap();
    assert_eq!(csr.mstatus, 0x0020_1889);
    // `csrc mstatus, t0` with t0 = MIE (IDF `riscv/vectors.S:184`).
    let clear = at(4, zicsr(CSR_MSTATUS, 3, 0, 5));
    assert_eq!(
        csr.access(&mut bus, clear, CSR_MSTATUS, CsrOp::Clear(MSTATUS_MIE)),
        Ok(0x0020_1889)
    );
    assert_eq!(csr.mstatus, 0x0020_1881);
    assert!(!csr.mstatus_mie());

    // Bit 11 decides both MPP bits; bit 12 alone stores nothing.
    csr.write(&mut bus, rw(5, CSR_MSTATUS), CSR_MSTATUS, 1 << 11)
        .unwrap();
    assert_eq!(csr.mstatus, MSTATUS_MPP);
    csr.write(&mut bus, rw(6, CSR_MSTATUS), CSR_MSTATUS, 1 << 12)
        .unwrap();
    assert_eq!(csr.mstatus, 0);
    assert!(bus.calls.is_empty());
}

#[test]
fn mtvec_is_forced_to_vectored_mode() {
    let mut bus = ClockBus::new();
    let mut h = hart(0x4200_0100);
    assert_eq!(
        h.csr.read(&mut bus, rw(0, CSR_MTVEC), CSR_MTVEC),
        Ok(1),
        "TRM reset value"
    );
    for (written, stored) in [
        (0x4000_1D01, 0x4000_1D01), // ROM
        (0x4038_0001, 0x4038_0001), // app `_vector_table | 1`
        (0x4038_0000, 0x4038_0001), // direct mode requested
        (0x4038_00FE, 0x4038_0001), // BASE is bits 31:8
    ] {
        h.csr
            .write(&mut bus, rw(1, CSR_MTVEC), CSR_MTVEC, written)
            .unwrap();
        // IDF reads it back in `esp_riscv_intr_num_reserved`.
        assert_eq!(
            h.csr.read(&mut bus, rw(2, CSR_MTVEC), CSR_MTVEC),
            Ok(stored),
            "{written:#x}"
        );
    }

    // Interrupt n enters at BASE + 4n, exceptions at BASE (IDF `riscv/vectors_intc.S:35-42`).
    h.csr.mstatus = MSTATUS_MIE;
    trap::take_interrupt(&mut h, 7);
    assert_eq!(h.pc, 0x4038_001C);
    assert_eq!(
        h.csr.read(&mut bus, rw(3, CSR_MCAUSE), CSR_MCAUSE),
        Ok(0x8000_0007)
    );
    assert_eq!(
        h.csr.read(&mut bus, rw(3, CSR_MEPC), CSR_MEPC),
        Ok(0x4200_0100)
    );
    trap::mret(&mut h);
    assert_eq!(h.pc, 0x4200_0100);
    assert!(h.csr.mstatus_mie());
    trap::take_exception(&mut h, 0x4200_0104, Trap::breakpoint(0x4200_0104));
    assert_eq!(h.pc, 0x4038_0000);
    assert!(bus.calls.is_empty());
}

#[test]
fn trap_csrs_and_mscratch_are_plain_registers() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    csr.write(&mut bus, rw(0, CSR_MEPC), CSR_MEPC, 0x4200_0003)
        .unwrap();
    assert_eq!(
        csr.read(&mut bus, rw(0, CSR_MEPC), CSR_MEPC),
        Ok(0x4200_0002),
        "bit 0 clear"
    );
    for n in [CSR_MCAUSE, CSR_MTVAL, CSR_MSCRATCH] {
        csr.write(&mut bus, rw(0, n), n, 0xFFFF_FFFF).unwrap();
        assert_eq!(csr.read(&mut bus, rw(0, n), n), Ok(0xFFFF_FFFF), "{n:#x}");
        let clear = CsrOp::Clear(0x8000_0000);
        assert_eq!(
            csr.access(&mut bus, rw(0, n), n, clear),
            Ok(0xFFFF_FFFF),
            "{n:#x}"
        );
    }
    assert_eq!(
        (csr.mcause, csr.mtval, csr.mscratch),
        (0x7FFF_FFFF, 0x7FFF_FFFF, 0x7FFF_FFFF)
    );
    // misa is WARL: writes are ignored rather than trapping.
    csr.write(&mut bus, rw(0, CSR_MISA), CSR_MISA, 0).unwrap();
    assert_eq!(
        csr.read(&mut bus, rw(0, CSR_MISA), CSR_MISA),
        Ok(MISA_VALUE)
    );
    assert!(bus.calls.is_empty());
}

/// IDF `PMP_ENTRY_SET` (`riscv/include/riscv/csr.h:124-127`): `csrw` pmpaddr, `csrs` pmpcfg.
fn pmp_entry_set(csr: &mut Csr, bus: &mut ClockBus, entry: u16, addr: u32, cfg: u8) {
    let n = CSR_PMPADDR0 + entry;
    csr.write(bus, at(0, zicsr(n, 1, 0, 5)), n, addr >> 2)
        .unwrap();
    let n = CSR_PMPCFG0 + entry / 4;
    let bits = u32::from(cfg) << ((entry % 4) * 8);
    csr.access(bus, at(0, zicsr(n, 2, 0, 5)), n, CsrOp::Set(bits))
        .unwrap();
}

#[test]
fn pmp_tor_with_lock_through_the_csr_path() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    // The first five entries of IDF `esp_cpu_configure_region_protection`
    // (`esp_hw_support/port/esp32c3/cpu_region_protect.c:32-57`, `soc/soc.h` addresses).
    let none = PMP_L | PMP_TOR;
    pmp_entry_set(&mut csr, &mut bus, 0, 0x2000_0000, none);
    pmp_entry_set(
        &mut csr,
        &mut bus,
        1,
        0x2800_0000,
        none | PMP_R | PMP_W | PMP_X,
    );
    pmp_entry_set(&mut csr, &mut bus, 2, 0x3C00_0000, none);
    pmp_entry_set(&mut csr, &mut bus, 3, 0x3FC8_0000, none | PMP_R);
    pmp_entry_set(&mut csr, &mut bus, 4, 0x3FCE_0000, none | PMP_R | PMP_W);
    let cfg0 = at(0, zicsr(CSR_PMPCFG0, 2, 10, 0));
    assert_eq!(csr.read(&mut bus, cfg0, CSR_PMPCFG0), Ok(0x8988_8F88));
    assert_eq!(csr.read(&mut bus, cfg0, CSR_PMPCFG0 + 1), Ok(0x0000_008B));
    assert_eq!(csr.read(&mut bus, cfg0, CSR_PMPADDR0 + 3), Ok(0x0FF2_0000));

    assert_eq!(bus.pmp_changes.len(), 10);
    assert_eq!(bus.pmp_changes.last(), Some(&(csr.pmpcfg, csr.pmpaddr)));
    assert!(bus.calls.is_empty(), "PMP CSRs never reach csr_custom");

    let pmp = Pmp::from_csr(&csr);
    let m = Privilege::Machine;
    assert!(
        pmp.check(0x3FC8_1000, 4, AccessKind::Write, m),
        "DRAM is RW"
    );
    assert!(
        !pmp.check(0x3FC8_1000, 4, AccessKind::Execute, m),
        "DRAM is not X"
    );
    assert!(pmp.check(0x3C00_0000, 4, AccessKind::Read, m), "DROM is R");
    assert!(
        !pmp.check(0x3C00_0000, 4, AccessKind::Write, m),
        "DROM is not W"
    );
    assert!(
        !pmp.check(0x0000_1000, 4, AccessKind::Read, m),
        "the NULL gap is locked NONE"
    );
    assert!(
        pmp.check(0x2000_0000, 2, AccessKind::Execute, m),
        "the debug region is RWX"
    );
    assert!(
        pmp.check(0x6000_0000, 4, AccessKind::Write, m),
        "no entry matches"
    );

    // Locked entries ignore writes, silently: no trap and no pmp_changed.
    let locked = [
        (CSR_PMPADDR0 + 3, CsrOp::Write(0), 0x0FF2_0000), // lower bound of locked TOR entry 4
        (CSR_PMPADDR0 + 4, CsrOp::Write(0), 0x0FF3_8000), // entry 4 itself
        (CSR_PMPCFG0, CsrOp::Clear(u32::MAX), 0x8988_8F88),
    ];
    for (n, op, value) in locked {
        let cx = at(0, zicsr(n, 1, 10, 5));
        assert_eq!(csr.access(&mut bus, cx, n, op), Ok(value), "{n:#x}");
        assert_eq!(csr.read(&mut bus, cx, n), Ok(value), "{n:#x}");
    }
    assert_eq!(bus.pmp_changes.len(), 10);

    // Unlocked bytes and addresses of the same CSRs still take writes.
    let cfg1 = at(0, zicsr(CSR_PMPCFG0 + 1, 1, 10, 5));
    let op = CsrOp::Write(0x0000_0F00);
    assert_eq!(csr.access(&mut bus, cfg1, CSR_PMPCFG0 + 1, op), Ok(0x8B));
    assert_eq!(csr.read(&mut bus, cfg1, CSR_PMPCFG0 + 1), Ok(0x0F8B));
    assert_eq!(bus.pmp_changes.len(), 11);
    let addr5 = at(0, zicsr(CSR_PMPADDR0 + 5, 1, 0, 5));
    csr.write(&mut bus, addr5, CSR_PMPADDR0 + 5, 0x1000_0000)
        .unwrap();
    assert_eq!(bus.pmp_changes.len(), 12);
    csr.write(&mut bus, addr5, CSR_PMPADDR0 + 5, 0x1000_0000)
        .unwrap();
    assert_eq!(
        bus.pmp_changes.len(),
        12,
        "an unchanged value is not a change"
    );
}

#[test]
fn trigger_csrs_store_data_per_selected_trigger() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    for (t, data) in [(0, 0x2800_1044), (1, 0x2800_0045)] {
        csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, t)
            .unwrap();
        csr.write(&mut bus, rw(0, CSR_TDATA1), CSR_TDATA1, data)
            .unwrap();
        csr.write(&mut bus, rw(0, CSR_TDATA2), CSR_TDATA2, 0x3FC8_0000 + t)
            .unwrap();
    }
    csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, 0)
        .unwrap();
    assert_eq!(
        csr.read(&mut bus, rw(0, CSR_TDATA1), CSR_TDATA1),
        Ok(0x2800_1044)
    );
    assert_eq!(
        csr.read(&mut bus, rw(0, CSR_TDATA2), CSR_TDATA2),
        Ok(0x3FC8_0000)
    );
    assert_eq!((csr.tdata1[1], csr.tdata2[1]), (0x2800_0045, 0x3FC8_0001));

    // Every hart trigger (IDF writes 0 to 7, `rv_utils.h:354-360`) keeps its own tdata pair.
    assert_eq!(TRIGGERS as u32, HART_TRIGGERS);
    for n in 2..HART_TRIGGERS {
        csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, n)
            .unwrap();
        assert_eq!(csr.read(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT), Ok(n));
        csr.write(&mut bus, rw(0, CSR_TDATA1), CSR_TDATA1, 0x2800_1040 + n)
            .unwrap();
        csr.write(&mut bus, rw(0, CSR_TDATA2), CSR_TDATA2, 0x3FC8_0000 + n)
            .unwrap();
    }
    for n in 2..HART_TRIGGERS {
        csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, n)
            .unwrap();
        assert_eq!(
            csr.read(&mut bus, rw(0, CSR_TDATA1), CSR_TDATA1),
            Ok(0x2800_1040 + n)
        );
        assert_eq!(
            csr.read(&mut bus, rw(0, CSR_TDATA2), CSR_TDATA2),
            Ok(0x3FC8_0000 + n)
        );
    }
    assert_eq!((csr.tdata1[1], csr.tdata2[1]), (0x2800_0045, 0x3FC8_0001));

    // A write of `HART_TRIGGERS` or more is ignored (UNVERIFIED WARL).
    let last = HART_TRIGGERS - 1;
    for n in [HART_TRIGGERS, u32::MAX] {
        csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, n)
            .unwrap();
        assert_eq!(
            csr.read(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT),
            Ok(last)
        );
    }
    csr.write(&mut bus, rw(0, CSR_TSELECT), CSR_TSELECT, 0)
        .unwrap();

    csr.write(&mut bus, rw(0, CSR_TCONTROL), CSR_TCONTROL, u32::MAX)
        .unwrap();
    assert_eq!(csr.tcontrol, TCONTROL_MTE | TCONTROL_MPTE);
    assert!(bus.calls.is_empty());
}

#[test]
fn identification_csrs_read_the_trm_values_and_reject_writes() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    let ids = [
        (CSR_MVENDORID, MVENDORID_VALUE),
        (CSR_MARCHID, MARCHID_VALUE),
        (CSR_MIMPID, MIMPID_VALUE),
        (CSR_MHARTID, 0),
    ];
    for (n, value) in ids {
        // `csrr` is `csrrs` with rs1 = x0: a read.
        let cx = at(0, zicsr(n, 2, 10, 0));
        assert_eq!(csr.read(&mut bus, cx, n), Ok(value), "{n:#x}");
        let write = at(0, zicsr(n, 1, 10, 11));
        let illegal = Trap::illegal_instruction(write.insn);
        assert_eq!(csr.write(&mut bus, write, n, value), Err(illegal), "{n:#x}");
        // `csrrs` with a register that holds 0 still writes (Zicsr).
        let set = CsrOp::Set(0);
        assert_eq!(csr.access(&mut bus, write, n, set), Err(illegal), "{n:#x}");
    }
    assert!(bus.calls.is_empty());
}

#[test]
fn an_illegal_csr_access_traps_precisely() {
    let mut bus = ClockBus::new();
    let mut h = hart(0x4200_0200);
    h.csr.mtvec = 0x4038_0001;
    h.csr.mstatus = MSTATUS_MIE;
    h.x[10] = 0x5555;
    for n in [CSR_MIE, CSR_MIP, CSR_DCSR, CSR_DSCRATCH1, CSR_MHARTID] {
        let insn = zicsr(n, 1, 10, 11);
        let err = h.csr.write(&mut bus, at(h.insns, insn), n, 1).unwrap_err();
        assert_eq!(err, Trap::illegal_instruction(insn), "{n:#x}");
    }

    let insn = zicsr(CSR_MIE, 2, 10, 0);
    let err = h
        .csr
        .read(&mut bus, at(h.insns, insn), CSR_MIE)
        .unwrap_err();
    let pc = h.pc;
    trap::take_exception(&mut h, pc, err);
    assert_eq!(h.pc, 0x4038_0000);
    assert_eq!(
        (h.csr.mepc, h.csr.mcause, h.csr.mtval),
        (pc, EXC_ILLEGAL_INSN, insn)
    );
    assert_eq!(h.csr.mstatus, MSTATUS_MPIE | MSTATUS_MPP);
    assert_eq!(
        (h.x[10], h.insns),
        (0x5555, 0),
        "the instruction did not complete"
    );
    assert!(bus.calls.is_empty());
}

#[test]
fn unknown_csrs_reach_the_bus_unless_strict() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    let lenient = at(40, zicsr(0x3C0, 2, 10, 0));
    assert_eq!(csr.read(&mut bus, lenient, 0x3C0), Ok(CUSTOM_VALUE));
    let gpio_oen = at(41, zicsr(0x803, 1, 10, 11));
    assert_eq!(csr.write(&mut bus, gpio_oen, 0x803, 0xF), Ok(()));
    assert_eq!(
        bus.calls,
        [(0x3C0, CsrOp::Read, 40), (0x803, CsrOp::Write(0xF), 41)]
    );

    // strict_csr: unknown standard numbers trap before the bus; custom ranges still reach it.
    let strict = CsrCx {
        strict: true,
        ..lenient
    };
    assert_eq!(
        csr.read(&mut bus, strict, 0x3C0),
        Err(Trap::illegal_instruction(strict.insn))
    );
    assert_eq!(bus.calls.len(), 2);
    let strict_custom = CsrCx {
        strict: true,
        ..gpio_oen
    };
    assert_eq!(csr.read(&mut bus, strict_custom, 0x803), Ok(CUSTOM_VALUE));
    assert_eq!(bus.calls.len(), 3);

    // A bus trap reaches mcause and mtval unchanged.
    bus.refuse = Some(0x805);
    let out = at(42, zicsr(0x805, 1, 10, 11));
    assert_eq!(csr.write(&mut bus, out, 0x805, 1), Err(REFUSED));
    bus.refuse = Some(CSR_MPCCR);
    let ccr = at(43, zicsr(CSR_MPCCR, 2, 10, 0));
    assert_eq!(csr.read(&mut bus, ccr, CSR_MPCCR), Err(REFUSED));

    // Only illegal instruction gets its mtval from the CPU: the bus never sees the instruction.
    bus.refuse = Some(0x805);
    bus.refuse_trap = Trap {
        cause: EXC_ILLEGAL_INSN,
        tval: 0xDEAD_BEEF,
    };
    assert_eq!(
        csr.write(&mut bus, out, 0x805, 1),
        Err(Trap::illegal_instruction(out.insn))
    );
}

/// A write, set or clear traps before the bus; a read reaches it (TRM section 1.4.1 table note).
#[test]
fn the_read_only_gpio_input_csr_rejects_writes() {
    let mut bus = ClockBus::new();
    let mut csr = Csr::new();
    let read = at(50, zicsr(CSR_CPU_GPIO_IN, 2, 10, 0));
    assert_eq!(csr.read(&mut bus, read, CSR_CPU_GPIO_IN), Ok(CUSTOM_VALUE));
    assert_eq!(bus.calls, [(CSR_CPU_GPIO_IN, CsrOp::Read, 50)]);

    let write = at(51, zicsr(CSR_CPU_GPIO_IN, 1, 10, 11));
    let illegal = Err(Trap::illegal_instruction(write.insn));
    for op in [CsrOp::Write(0), CsrOp::Set(0), CsrOp::Clear(0)] {
        assert_eq!(csr.access(&mut bus, write, CSR_CPU_GPIO_IN, op), illegal);
    }
    // Even in lenient mode; the writable neighbors still take writes.
    assert_eq!(bus.calls.len(), 1, "no write reached the bus");
    for n in [0x803, 0x805] {
        assert_eq!(
            csr.write(&mut bus, at(52, zicsr(n, 1, 10, 11)), n, 3),
            Ok(())
        );
    }
}
