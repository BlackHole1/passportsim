//! Milestone M0 tests: the decoder, the engine against `ref_step`, the ESP CSRs, the scheduler,
//! the clock formulas, the snapshot codec and `doctor`. Names use the prefix `t<tier>_m0_` so
//! `xtask ci --milestone 0` can count them. The T1 half skips with a printed reason without the
//! corpus or the `riscv32-esp-elf` objdump.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use pemu_core::clock::{
    Clock, DEFAULT_CPI_MILLI, DEFAULT_CPU_HZ, SYSTIMER_COUNTER_MASK, SYSTIMER_TICK_PS,
    ps_per_cycle, systimer_count, systimer_deadline,
};
use pemu_core::rng::{DetRng, RngStream};
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::snap::{Codec, SectionId, SnapSection};
use pemu_core::time::{VTime, frame_time};
use pemu_loader::elf::{ElfInfo, SHF_EXECINSTR};
use pemu_loader::rom::{self, RomRev};
use pemu_rv32::bus::{Access, Bus, CodePage, HartView, PF_CODE, PF_R, PF_W, PF_X, PageTable};
use pemu_rv32::csr::{
    CSR_MPCCR, CSR_MPCER, CSR_MPCMR, CSR_MTVEC, CSR_UPCCR, CSR_UPCER, CSR_UPCMR, CSR_USTATUS, Csr,
    CsrEffect, CsrOp, MPCER_CYCLE, MPCMR_COUNT_EN, MSTATUS_MIE,
};
use pemu_rv32::decode::decode_at;
use pemu_rv32::disasm::format_op;
use pemu_rv32::engine::{Engine, EngineCfg, Exit, HookSet};
use pemu_rv32::exec::Hart;
use pemu_rv32::op::{K_FENCEI, K_NOP};
use pemu_rv32::pmp::{AccessKind, CSR_PMPADDR0, CSR_PMPCFG0, PMP_L, PMP_TOR, Pmp, Privilege};
use pemu_rv32::refstep::{StepResult, ref_step};
use pemu_rv32::spmon::{SpMonitor, SpSpill};
use pemu_rv32::trap::{self, Trap};

const BLOCK_SIZES: [u16; 3] = [1, 3, 64];

/// How often the CSR test's machine makes the hart ask before it reports a wake, so the wait is
/// one the run loop sat through (`Bus::wfi_wake`).
const WFI_POLLS_BEFORE_WAKE: u64 = 4;

/// Virtual base of the test machine: SRAM1 through the instruction bus.
const BASE: u32 = 0x4038_0000;
const PAGE: u32 = 4096;
const PAGES: u32 = 4;
/// Pages the pc may reach; `fetch_code` refuses the rest, so the engine never translates a page a
/// fast store could write.
const CODE_PAGES: u32 = 2;
const VECTOR: u32 = BASE + PAGE;

// ------------------------------------------------------------------------------------------------
// The machine
// ------------------------------------------------------------------------------------------------

/// A flat machine with two code pages and two data pages.
///
/// The code pages carry `PF_CODE`, so every store into them takes the slow path, answers `OkStop`
/// and is invalidated by the driver in the SoC's place. The data pages run the inlined fast
/// paths, which `ref_step` never takes and which are therefore worth comparing.
struct M0Bus {
    arena: Vec<u8>,
    pages: PageTable,
    custom: BTreeMap<u16, u32>,
    spmon: SpMonitor,
    dirty: Vec<u32>,
    /// The ESP performance counters, when the test asked for them. `None` elsewhere, so the
    /// interpreter comparisons see the plain `custom` store.
    clock: Option<CycleClock>,
    clock_calls: Vec<(u16, CsrOp, u64)>,
    wfi_polls: u64,
}

/// The CPU cycle counter the SoC derives from `Clock`, reduced to CPI 1.
///
/// One count per retired instruction while `mpcer.CYCLE` and `mpcmr.COUNT_EN` are both set, and
/// nothing without a retired instruction, which stops it in WFI (TRM Register 1.12). Every enable
/// change and every write to the counter rebases it.
#[derive(Clone, Copy, Debug, Default)]
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

impl M0Bus {
    fn new(words: &[u32], spmon: SpMonitor) -> M0Bus {
        let mut arena = vec![0u8; (PAGES * PAGE) as usize];
        for (i, w) in words.iter().enumerate() {
            arena[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        // The trap handler: `mepc += 4`, `mret`, so a trap skips the instruction that raised it.
        let at = (VECTOR - BASE) as usize;
        for (i, w) in trap_handler().iter().enumerate() {
            arena[at + i * 4..at + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        let mut pages = PageTable::new();
        for p in 0..PAGES {
            let flags = if p < CODE_PAGES {
                PF_R | PF_W | PF_X | PF_CODE
            } else {
                PF_R | PF_W
            };
            pages.set_entry((BASE >> 12) + p, (p * PAGE) | flags);
        }
        M0Bus {
            arena,
            pages,
            custom: BTreeMap::new(),
            spmon,
            dirty: Vec::new(),
            clock: None,
            clock_calls: Vec::new(),
            wfi_polls: 0,
        }
    }

    /// The same machine with the ESP performance counters modelled. They start disabled, as out of
    /// reset (TRM Register 1.12).
    fn with_cycle_counter(mut self) -> M0Bus {
        self.clock = Some(CycleClock::default());
        self
    }

    fn offset(&self, addr: u32, size: u8) -> Option<usize> {
        let off = addr.checked_sub(BASE)? as usize;
        (off + usize::from(size) <= self.arena.len()).then_some(off)
    }
}

impl Bus for M0Bus {
    fn pages(&self) -> &PageTable {
        &self.pages
    }
    fn arena(&mut self) -> *mut u8 {
        self.arena.as_mut_ptr()
    }
    fn load_slow(&mut self, addr: u32, size: u8, _hart: &HartView) -> Access<u32> {
        match self.offset(addr, size) {
            Some(off) => {
                let mut v = 0u32;
                for i in (0..usize::from(size)).rev() {
                    v = (v << 8) | u32::from(self.arena[off + i]);
                }
                Access::Ok(v)
            }
            None => Access::Fault(Trap::load_access_fault(addr)),
        }
    }
    fn store_slow(&mut self, addr: u32, size: u8, val: u32, _hart: &HartView) -> Access<()> {
        let Some(off) = self.offset(addr, size) else {
            return Access::Fault(Trap::store_access_fault(addr));
        };
        for i in 0..usize::from(size) {
            self.arena[off + i] = (val >> (8 * i)) as u8;
        }
        if off as u32 / PAGE < CODE_PAGES {
            self.dirty.push(addr);
            return Access::OkStop(());
        }
        Access::Ok(())
    }
    fn sp_monitor(&self) -> SpMonitor {
        self.spmon
    }
    fn fetch_code(&mut self, vaddr: u32) -> Result<CodePage<'_>, Trap> {
        let off = self
            .offset(vaddr, 1)
            .ok_or(Trap::instruction_access_fault(vaddr))?;
        if off as u32 / PAGE >= CODE_PAGES {
            return Err(Trap::instruction_access_fault(vaddr));
        }
        let end = (off / PAGE as usize + 1) * PAGE as usize;
        Ok(CodePage {
            bytes: &self.arena[off..end],
        })
    }
    fn csr_custom(&mut self, csr: u16, op: CsrOp, insns: u64) -> Result<(u32, CsrEffect), Trap> {
        if let Some(clock) = self.clock.as_mut()
            && matches!(
                csr,
                CSR_MPCER | CSR_MPCMR | CSR_MPCCR | CSR_UPCER | CSR_UPCMR | CSR_UPCCR
            )
        {
            self.clock_calls.push((csr, op, insns));
            return Ok((
                match csr {
                    // 0x7E0/0x800 and 0x7E1/0x801 are stored by the CPU; the SoC only rebases.
                    CSR_MPCER | CSR_UPCER => {
                        clock.rebase(insns, clock.count(insns));
                        if let Some(new) = op.new_value(clock.pcer) {
                            clock.pcer = new;
                        }
                        u32::MAX
                    }
                    CSR_MPCMR | CSR_UPCMR => {
                        clock.rebase(insns, clock.count(insns));
                        if let Some(new) = op.new_value(clock.pcmr) {
                            clock.pcmr = new;
                        }
                        u32::MAX
                    }
                    // 0x7E2/0x802 is not stored by the CPU: the value comes from here.
                    _ => {
                        let old = clock.count(insns) as u32;
                        if let Some(new) = op.new_value(old) {
                            clock.rebase(insns, u64::from(new));
                        }
                        old
                    }
                },
                CsrEffect::None,
            ));
        }
        let slot = self.custom.entry(csr).or_default();
        let before = *slot;
        if let CsrOp::Write(v) = op {
            *slot = v;
        }
        Ok((before, CsrEffect::None))
    }
    fn wfi_wake(&mut self) -> bool {
        self.wfi_polls += 1;
        // The CSR machine keeps the hart waiting a few polls before it wakes it; every other test
        // drives `Hart::wfi` itself.
        self.clock.is_some() && self.wfi_polls.is_multiple_of(WFI_POLLS_BEFORE_WAKE)
    }
    fn pmp_changed(&mut self, _csr: &Csr) {}
}

fn hart(spmon: SpMonitor, sp: u32) -> Hart {
    let mut csr = Csr::new();
    csr.mtvec = VECTOR | 1;
    let mut x = [0u32; 32];
    x[2] = sp;
    x[3] = BASE + 2 * PAGE; // a pointer into the first data page
    Hart {
        x,
        pc: BASE,
        csr,
        wfi: false,
        insns: 0,
        stores: 0,
        spmon,
        extra: 0,
        pipe: Default::default(),
    }
}

// ------------------------------------------------------------------------------------------------
// Instruction encoders (RISC-V unprivileged specification, RV32I formats)
// ------------------------------------------------------------------------------------------------

fn i_type(imm: i32, rs1: u32, funct3: u32, rd: u32, op: u32) -> u32 {
    (((imm as u32) & 0xFFF) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | op
}

fn r_type(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32) -> u32 {
    (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x33
}

fn s_type(imm: i32, rs2: u32, rs1: u32, funct3: u32) -> u32 {
    let imm = (imm as u32) & 0xFFF;
    ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | ((imm & 31) << 7) | 0x23
}

fn b_type(off: i32, rs2: u32, rs1: u32, funct3: u32) -> u32 {
    let o = off as u32;
    (((o >> 12) & 1) << 31)
        | (((o >> 5) & 0x3F) << 25)
        | (rs2 << 20)
        | (rs1 << 15)
        | (funct3 << 12)
        | (((o >> 1) & 0xF) << 8)
        | (((o >> 11) & 1) << 7)
        | 0x63
}

fn j_type(off: i32, rd: u32) -> u32 {
    let o = off as u32;
    (((o >> 20) & 1) << 31)
        | (((o >> 1) & 0x3FF) << 21)
        | (((o >> 11) & 1) << 20)
        | (((o >> 12) & 0xFF) << 12)
        | (rd << 7)
        | 0x6F
}

fn trap_handler() -> [u32; 4] {
    [
        i_type(0x341, 0, 2, 30, 0x73),
        i_type(4, 30, 0, 30, 0x13),
        i_type(0x341, 30, 1, 0, 0x73),
        0x3020_0073,
    ]
}

// ------------------------------------------------------------------------------------------------
// Comparing the two interpreters
// ------------------------------------------------------------------------------------------------

/// Everything the interpreter comparisons compare: registers, pc, counters, the stack-monitor
/// mirror, every C3 CSR, the unknown CSRs the bus stored, and memory.
#[derive(PartialEq, Eq)]
struct State {
    x: [u32; 32],
    pc: u32,
    insns: u64,
    stores: u64,
    wfi: bool,
    spmon: (bool, bool, u32, u32),
    csr: Vec<u32>,
    custom: Vec<(u16, u32)>,
    memory: Vec<u8>,
}

fn state(hart: &Hart, bus: &M0Bus) -> State {
    let c = &hart.csr;
    let mut csr = vec![
        c.mstatus, c.mtvec, c.mepc, c.mcause, c.mtval, c.mscratch, c.tselect, c.tcontrol, c.mpcer,
        c.mpcmr, c.csr000,
    ];
    csr.extend(c.pmpcfg.iter().map(|b| u32::from(*b)));
    csr.extend(c.pmpaddr);
    csr.extend(c.tdata1);
    csr.extend(c.tdata2);
    State {
        x: hart.x,
        pc: hart.pc,
        insns: hart.insns,
        stores: hart.stores,
        wfi: hart.wfi,
        spmon: (
            hart.spmon.on_min,
            hart.spmon.on_max,
            hart.spmon.min,
            hart.spmon.max,
        ),
        csr,
        custom: bus.custom.iter().map(|(k, v)| (*k, *v)).collect(),
        memory: bus.arena.clone(),
    }
}

fn difference(a: &State, b: &State) -> String {
    for (i, (x, y)) in a.x.iter().zip(b.x.iter()).enumerate() {
        if x != y {
            return format!("x{i}: reference 0x{x:08x}, engine 0x{y:08x}");
        }
    }
    if a.pc != b.pc {
        return format!("pc: reference 0x{:08x}, engine 0x{:08x}", a.pc, b.pc);
    }
    if a.insns != b.insns {
        return format!("insns: reference {}, engine {}", a.insns, b.insns);
    }
    if a.stores != b.stores {
        return format!("stores: reference {}, engine {}", a.stores, b.stores);
    }
    if a.spmon != b.spmon {
        return format!("spmon: reference {:?}, engine {:?}", a.spmon, b.spmon);
    }
    for (i, (x, y)) in a.csr.iter().zip(b.csr.iter()).enumerate() {
        if x != y {
            return format!("csr digest slot {i}: reference 0x{x:08x}, engine 0x{y:08x}");
        }
    }
    if a.custom != b.custom {
        return format!("unknown CSRs: {:x?} against {:x?}", a.custom, b.custom);
    }
    for (i, (x, y)) in a.memory.iter().zip(b.memory.iter()).enumerate() {
        if x != y {
            return format!(
                "memory at 0x{:08x}: reference 0x{x:02x}, engine 0x{y:02x}",
                BASE + i as u32
            );
        }
    }
    "the states differ in a field the reporter does not cover".to_string()
}

fn reference(program: &[u32], retire: u64, spmon: SpMonitor, sp: u32) -> (Hart, M0Bus) {
    let mut hart = hart(spmon, sp);
    let mut bus = M0Bus::new(program, spmon);
    while hart.insns < retire {
        if let StepResult::Wfi = ref_step(&mut hart, &mut bus) {
            hart.wfi = false;
        }
    }
    (hart, bus)
}

/// Runs `program` through the engine at `max_block_insns` until it has retired `retire`
/// instructions, invalidating in the SoC's place after every run.
fn engine(
    program: &[u32],
    retire: u64,
    max_block_insns: u16,
    spmon: SpMonitor,
    sp: u32,
) -> (Hart, M0Bus, Engine) {
    let mut hart = hart(spmon, sp);
    let mut bus = M0Bus::new(program, spmon);
    let mut engine = Engine::new(EngineCfg {
        max_block_insns,
        ..EngineCfg::default()
    });
    let hooks = HookSet::default();
    while hart.insns < retire {
        let budget = retire - hart.insns;
        let exit = engine.run(&mut hart, &mut bus, &hooks, budget);
        for addr in std::mem::take(&mut bus.dirty) {
            engine.invalidate_vrange(addr, 1);
        }
        match exit {
            Exit::Wfi => hart.wfi = false,
            Exit::Budget | Exit::Stop | Exit::SpSpill(_) => {}
            other => panic!("the engine ended with {other:?}"),
        }
    }
    (hart, bus, engine)
}

/// A program covering every class the engine treats differently: computation inside a block, a
/// fast-path memory access, a store into a code page (`OkStop`), each terminator kind, a taken
/// and a not-taken branch, a CSR access and a trap.
fn mixed_program() -> Vec<u32> {
    vec![
        i_type(7, 0, 0, 1, 0x13),      //  0 addi x1, x0, 7
        i_type(-3, 1, 0, 4, 0x13),     //  1 addi x4, x1, -3
        r_type(1, 4, 1, 0, 5),         //  2 mul x5, x1, x4
        r_type(1, 4, 1, 4, 6),         //  3 div x6, x1, x4
        r_type(0x20, 4, 1, 0, 7),      //  4 sub x7, x1, x4
        s_type(0, 5, 3, 2),            //  5 sw x5, 0(x3)    data page, fast store
        i_type(0, 3, 2, 8, 0x03),      //  6 lw x8, 0(x3)    data page, fast load
        i_type(1, 3, 4, 9, 0x03),      //  7 lbu x9, 1(x3)
        s_type(2, 9, 3, 1),            //  8 sh x9, 2(x3)
        b_type(8, 8, 5, 0),            //  9 beq x5, x8, +8  taken
        i_type(0x55, 0, 0, 10, 0x13),  // 10 addi x10, x0, 0x55  (skipped)
        i_type(0x340, 1, 1, 11, 0x73), // 11 csrrw x11, mscratch, x1
        i_type(0x340, 0, 2, 12, 0x73), // 12 csrrs x12, mscratch, x0
        b_type(8, 0, 1, 0),            // 13 beq x1, x0, +8  not taken
        i_type(0x31, 0, 0, 13, 0x13),  // 14 addi x13, x0, 0x31
        j_type(8, 14),                 // 15 jal x14, +8
        i_type(0x66, 0, 0, 15, 0x13),  // 16 addi x15, x0, 0x66  (skipped)
        0x0000_0073,                   // 17 ecall  (traps; the handler skips it)
        i_type(0x77, 0, 0, 16, 0x13),  // 18 addi x16, x0, 0x77
        i_type(0, 0, 0, 0, 0x0F),      // 19 fence  (a NOP)
        0x0000_100F,                   // 20 fence.i  (flushes the cache)
        i_type(0x88, 0, 0, 17, 0x13),  // 21 addi x17, x0, 0x88
        s_type(0, 17, 0, 2),           // 22 sw x17, 0(x0)   unmapped: a store fault
        i_type(0x99, 0, 0, 18, 0x13),  // 23 addi x18, x0, 0x99
        j_type(0, 0),                  // 24 jal x0, 0       a self-loop
    ]
}

// ------------------------------------------------------------------------------------------------
// The M0 claims
// ------------------------------------------------------------------------------------------------

/// The engine at `max_block_insns` 1, 3 and 64 matches the reference interpreter after every
/// single instruction, on every register, CSR, store and trap.
#[test]
fn t0_m0_the_engine_agrees_with_ref_step_at_block_sizes_1_3_and_64() {
    let program = mixed_program();
    let monitor = SpMonitor::default();
    for retire in 1..=30u64 {
        let (ref_hart, ref_bus) = reference(&program, retire, monitor, 0);
        let expected = state(&ref_hart, &ref_bus);
        for max in BLOCK_SIZES {
            let (hart, bus, _) = engine(&program, retire, max, monitor, 0);
            let got = state(&hart, &bus);
            assert!(
                expected == got,
                "after {retire} instructions at max_block_insns {max}: {}",
                difference(&expected, &got)
            );
        }
    }
    // Not vacuous: the program reached its trap, its CSR writes and its stores.
    let (hart, bus) = reference(&program, 30, monitor, 0);
    assert_eq!(hart.csr.mcause, 7, "the store to 0 raised a store fault");
    assert_eq!(hart.csr.mscratch, 7, "the CSR write landed");
    // Two stores retired (the faulting one to address 0 counts none).
    assert_eq!(hart.stores, 2, "the program's stores landed");
    assert_ne!(
        bus.arena[(2 * PAGE) as usize],
        0,
        "the data page was written"
    );
}

/// The same agreement over seeded random programs.
///
/// A small count here; the 10^4 (T0) and 10^6 (T2) runs live in `pemu-rv32/tests/fuzz.rs`. A
/// failure is reproduced from the seed the assertion prints.
#[test]
fn t0_m0_the_engine_agrees_with_ref_step_over_seeded_random_programs() {
    for iteration in 0..64u64 {
        let seed = 0x4D30_5F45_302E_3400 ^ iteration.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut rng = DetRng::new(seed);
        let mut draw = || rng.stream(RngStream(0)).next_u32();
        let n = 8 + (draw() % 56) as usize;
        let mut program = Vec::with_capacity(n);
        for i in 0..n {
            let rd = 1 + draw() % 31;
            let rs1 = draw() % 8;
            let rs2 = draw() % 8;
            let imm = (draw() % 64) as i32 - 32;
            program.push(match draw() % 8 {
                0 => i_type(imm, rs1, 0, rd, 0x13),
                1 => r_type(0, rs2, rs1, 0, rd),
                2 => r_type(1, rs2, rs1, 0, rd),
                3 => i_type(imm & 0xFC, 3, 2, rd, 0x03),
                4 => s_type(imm & 0xFC, rs2, 3, 2),
                5 => b_type(4 * ((draw() % n as u32) as i32 - i as i32), rs2, rs1, 0),
                6 => j_type(4 * ((draw() % n as u32) as i32 - i as i32), rd),
                _ => draw() & 0xFFFF, // a raw halfword: compressed, hint or illegal
            });
        }
        let retire = 1 + u64::from(draw() % 40);
        let (ref_hart, ref_bus) = reference(&program, retire, SpMonitor::default(), 0);
        let expected = state(&ref_hart, &ref_bus);
        for max in BLOCK_SIZES {
            let (hart, bus, _) = engine(&program, retire, max, SpMonitor::default(), 0);
            let got = state(&hart, &bus);
            assert!(
                expected == got,
                "seed 0x{seed:016x} at max_block_insns {max}, {retire} instructions: {}",
                difference(&expected, &got)
            );
        }
    }
}

/// Exact budgets: a run retires exactly `max_insns`, and a run stopped inside a block re-enters
/// it through a resume token instead of translating a new block.
///
/// Without the token an exact-deadline engine translates 352 blocks instead of 180 on fw-Og
/// (design-facts perf-A A.4).
#[test]
fn t0_m0_exact_budgets_retire_the_whole_budget_and_resume_in_block() {
    let mut program: Vec<u32> = (0..63).map(|_| i_type(1, 1, 0, 1, 0x13)).collect();
    program.push(j_type(0, 0));

    for budget in [1u64, 2, 7, 63] {
        let (hart, _, _) = engine(&program, budget, 64, SpMonitor::default(), 0);
        assert_eq!(hart.insns, budget, "budget {budget}");
        assert_eq!(u64::from(hart.x[1]), budget);
        assert_eq!(hart.pc, BASE + 4 * budget as u32);
    }

    // Sixty one-instruction slices: one translation, every later slice re-enters it.
    let mut hart = hart(SpMonitor::default(), 0);
    let mut bus = M0Bus::new(&program, SpMonitor::default());
    let mut engine = Engine::new(EngineCfg {
        max_block_insns: 64,
        ..EngineCfg::default()
    });
    let hooks = HookSet::default();
    for _ in 0..60 {
        assert_eq!(
            engine.run(&mut hart, &mut bus, &hooks, 1),
            Exit::Budget,
            "a one-instruction slice ends on its budget"
        );
    }
    let stats = engine.stats();
    assert_eq!(hart.insns, 60);
    assert_eq!(
        stats.blocks_built, 1,
        "one translation for sixty one-instruction slices, not sixty"
    );
    assert_eq!(stats.partial_blocks, 60);
    assert_eq!(stats.resumes, 59);
}

/// The stack guard is exact: an op flagged `F_WRITES_SP` runs the monitor check after its write,
/// and a violation ends the block after that instruction, at the same count for every block
/// size, slice and stop pattern.
#[test]
fn t0_m0_the_stack_guard_trips_at_the_same_instruction_at_every_block_size() {
    let sp_base = BASE + 2 * PAGE + 0x400;
    let monitor = SpMonitor {
        on_min: true,
        on_max: false,
        min: sp_base - 0x200,
        max: 0,
    };
    // The third `addi sp, sp, -0x100` takes `sp` below the bound (bounds are inclusive).
    let program = vec![i_type(-0x100, 2, 0, 2, 0x13); 4];

    for max in BLOCK_SIZES {
        for slice in [1u64, 2, 8] {
            let mut hart = hart(monitor, sp_base);
            let mut bus = M0Bus::new(&program, monitor);
            let mut engine = Engine::new(EngineCfg {
                max_block_insns: max,
                ..EngineCfg::default()
            });
            let hooks = HookSet::default();
            let mut spilled_at = None;
            while hart.insns < 4 && spilled_at.is_none() {
                if let Exit::SpSpill(spill) = engine.run(&mut hart, &mut bus, &hooks, slice) {
                    assert_eq!(spill, SpSpill::Min);
                    spilled_at = Some(hart.insns);
                }
            }
            assert_eq!(
                spilled_at,
                Some(3),
                "max_block_insns {max}, slice {slice}: the third write is the one below the bound"
            );
        }
    }
}

/// A store into a translated byte range invalidates that page's blocks, so self-modifying code
/// runs the instruction the guest wrote.
#[test]
fn t0_m0_self_modifying_code_runs_the_new_instruction_at_every_block_size() {
    let program = vec![i_type(1, 1, 0, 1, 0x13), j_type(-4, 0)];
    for max in BLOCK_SIZES {
        let mut hart = hart(SpMonitor::default(), 0);
        let mut bus = M0Bus::new(&program, SpMonitor::default());
        let mut engine = Engine::new(EngineCfg {
            max_block_insns: max,
            ..EngineCfg::default()
        });
        let hooks = HookSet::default();
        engine.run(&mut hart, &mut bus, &hooks, 6);
        assert_eq!(hart.x[1], 3, "max {max}: three turns at +1");

        // The bus answers `OkStop` because the page carries `PF_CODE`, and the driver invalidates.
        bus.arena[..4].copy_from_slice(&i_type(16, 1, 0, 1, 0x13).to_le_bytes());
        bus.dirty.push(BASE);
        for addr in std::mem::take(&mut bus.dirty) {
            engine.invalidate_vrange(addr, 4);
        }
        engine.run(&mut hart, &mut bus, &hooks, 6);
        assert_eq!(hart.x[1], 51, "max {max}: three more turns at +16");
    }

    // The other half: the guest's own store must reach the bus. A fast store into a translated page
    // would leave the old instruction cached, so `PF_CODE` is off the fast-store mask and the slow
    // path answers `OkStop`.
    let rewritten = i_type(16, 1, 0, 1, 0x13);
    let program = vec![
        u_type((rewritten >> 12) + u32::from(rewritten & 0x800 != 0), 5),
        i_type(((rewritten & 0xFFF) as i32) << 20 >> 20, 5, 0, 5, 0x13),
        u_type(BASE >> 12, 6), // BASE has no low 12 bits, so one `lui` holds it
        s_type(16, 5, 6, 2),   // sw x5, 16(x6): rewrite the `addi` four instructions on
        i_type(1, 1, 0, 1, 0x13),
        j_type(-4, 0),
    ];
    for max in BLOCK_SIZES {
        let mut hart = hart(SpMonitor::default(), 0);
        let mut bus = M0Bus::new(&program, SpMonitor::default());
        let mut engine = Engine::new(EngineCfg {
            max_block_insns: max,
            ..EngineCfg::default()
        });
        let hooks = HookSet::default();
        let mut invalidated = 0;
        while hart.insns < 10 {
            let budget = 10 - hart.insns;
            engine.run(&mut hart, &mut bus, &hooks, budget);
            for addr in std::mem::take(&mut bus.dirty) {
                assert_eq!(
                    addr,
                    BASE + 16,
                    "max {max}: the guest store reached the bus"
                );
                engine.invalidate_vrange(addr, 4);
                invalidated += 1;
            }
        }
        assert_eq!(
            invalidated, 1,
            "max {max}: the store into the code page took the slow path exactly once"
        );
        assert_eq!(
            u32::from_le_bytes(bus.arena[16..20].try_into().unwrap()),
            rewritten,
            "max {max}: the guest wrote the new instruction"
        );
        // Ten instructions: four to build and store, then three turns of (+16, jump).
        assert_eq!(hart.x[1], 48, "max {max}: three turns at +16");
    }
}

// ------------------------------------------------------------------------------------------------
// The decode corpus
// ------------------------------------------------------------------------------------------------
//
// Every instruction in the text sections of the corpus ELFs must decode to what
// `riscv32-esp-elf-objdump -d` prints, with 0 mismatches. `rom3` and `rom101` are bundled in
// `assets/rom/` (T0); the firmware ELFs and `rom0` live under the data root (T1, skipped with a
// reason where absent).
//
// Per file: coverage (the decoded lengths tile every executable section, no objdump needed) and
// agreement (this test's bytes equal objdump's hex column, and `format_op` equals its text).
// Operand fields are compared in `pemu-rv32/tests/decode_corpus.rs`. Data lines are skipped and
// counted; `.insn` is compared.

/// `assets/rom/` of this checkout. `CARGO_MANIFEST_DIR` is `tests/milestones/`.
fn assets_rom() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/rom")
}

/// The RISC-V objdump: `PASSPORTSIM_OBJDUMP`, else the toolchain under `$HOME/.espressif`, else
/// `PATH`, as `pemu-rv32/tests/decode_corpus.rs` looks.
fn objdump() -> PathBuf {
    if let Some(bin) = std::env::var_os("PASSPORTSIM_OBJDUMP") {
        return PathBuf::from(bin);
    }
    let bundled = std::env::var_os("HOME").map(|home| {
        PathBuf::from(home).join(
            ".espressif/tools/riscv32-esp-elf/esp-14.2.0_20251107/riscv32-esp-elf/bin/\
             riscv32-esp-elf-objdump",
        )
    });
    match bundled {
        Some(path) if path.is_file() => path,
        _ => PathBuf::from("riscv32-esp-elf-objdump"),
    }
}

/// One file of the decode corpus: its id, its path and the least number of instructions it must
/// yield, so a toolchain change cannot pass with nothing compared.
struct ElfTarget {
    id: String,
    path: PathBuf,
    min_insns: usize,
}

struct DisasmLine {
    addr: u32,
    bits: u32,
    len: usize,
    text: String,
    /// True for `.word`, `.byte` and `.short`; `.insn` is an instruction.
    data: bool,
}

/// Parses one line of `objdump -d`; `None` for elisions, headers and the out-of-bounds message.
///
/// The tab after the mnemonic becomes one space, a trailing ` # <addr> <sym>` comment is dropped
/// (objdump resolves `gp` and `lui` pairs there), and a trailing ` <symbol+offset>` on a branch
/// target is dropped (no symbol table is loaded).
fn parse_objdump_line(line: &str) -> Option<DisasmLine> {
    let mut fields = line.trim_start().splitn(4, '\t');
    let addr = u32::from_str_radix(fields.next()?.strip_suffix(':')?, 16).ok()?;
    let bits_text = fields.next()?.trim();
    if bits_text.len() != 4 && bits_text.len() != 8 {
        return None;
    }
    let bits = u32::from_str_radix(bits_text, 16).ok()?;
    let mnemonic = fields.next()?.trim();
    let operands = fields.next().unwrap_or("");
    let operands = operands.split(" #").next().unwrap_or("").trim();
    let operands = match operands.rfind(" <") {
        Some(at) if operands.ends_with('>') => &operands[..at],
        _ => operands,
    };
    Some(DisasmLine {
        addr,
        bits,
        len: bits_text.len() / 2,
        text: if operands.is_empty() {
            mnemonic.to_string()
        } else {
            format!("{mnemonic} {operands}")
        },
        data: mnemonic.starts_with('.') && mnemonic != ".insn",
    })
}

/// The executable sections of an ELF, in address order, as `(name, addr, bytes)`, read with
/// `pemu_loader::elf`, the emulator's own ELF reader.
fn exec_sections(info: &ElfInfo, file: &[u8]) -> Vec<(String, u32, Vec<u8>)> {
    let mut out: Vec<(String, u32, Vec<u8>)> = info
        .sections
        .iter()
        .filter(|s| s.flags & SHF_EXECINSTR != 0 && s.has_bits() && s.size > 0)
        .filter_map(|s| s.data(file).map(|d| (s.name.clone(), s.addr, d.to_vec())))
        .collect();
    out.sort_by_key(|(_, addr, _)| *addr);
    out
}

/// Walks every executable section end to end and decodes every instruction in it.
///
/// Returns the count, or the first place the walk broke: a decode that returned `None`, or a
/// cursor that stepped past the section end. Either means the lengths do not tile the section.
fn decode_every_instruction(info: &ElfInfo, file: &[u8]) -> Result<usize, String> {
    let mut decoded = 0usize;
    for (name, addr, bytes) in exec_sections(info, file) {
        let mut at = 0usize;
        while at < bytes.len() {
            let pc = addr.wrapping_add(at as u32);
            let op = decode_at(&bytes[at..], pc).ok_or_else(|| {
                format!(
                    "{name}: no decode at {pc:#010x} ({} byte(s) left)",
                    bytes.len() - at
                )
            })?;
            at += usize::from(op.len);
            decoded += 1;
        }
        if at != bytes.len() {
            return Err(format!(
                "{name}: the decoded lengths overrun the section by {} byte(s)",
                at - bytes.len()
            ));
        }
    }
    Ok(decoded)
}

struct DecodeCounts {
    id: String,
    walked: usize,
    /// Instruction lines compared with objdump; 0 when objdump did not run.
    compared: usize,
    data: usize,
    details: Vec<String>,
    mismatches: usize,
}

const MAX_DETAILS: usize = 10;

/// Compares one ELF against `objdump -d`, when `tool` is `Some`. The bytes come from this test's
/// own read and must equal objdump's hex column, or the comparison would be objdump with itself.
fn compare_one(target: &ElfTarget, tool: Option<&Path>) -> Result<DecodeCounts, String> {
    let file = std::fs::read(&target.path).map_err(|e| format!("cannot be read: {e}"))?;
    let info =
        ElfInfo::parse(&file).map_err(|e| format!("is not an ELF this loader reads: {e}"))?;
    let walked = decode_every_instruction(&info, &file)?;
    let mut counts = DecodeCounts {
        id: target.id.clone(),
        walked,
        compared: 0,
        data: 0,
        details: Vec::new(),
        mismatches: 0,
    };
    let Some(tool) = tool else {
        return Ok(counts);
    };
    let out = Command::new(tool)
        .arg("-d")
        .arg(&target.path)
        .output()
        .map_err(|e| format!("objdump did not run: {e}"))?;
    if !out.status.success() {
        return Err(format!("objdump exited with {}", out.status));
    }
    let listing = String::from_utf8_lossy(&out.stdout).into_owned();
    let sections = exec_sections(&info, &file);
    for line in listing.lines() {
        let Some(line) = parse_objdump_line(line) else {
            continue;
        };
        if line.data {
            counts.data += 1;
            continue;
        }
        let mut fail = |detail: String| {
            counts.mismatches += 1;
            if counts.details.len() < MAX_DETAILS {
                counts.details.push(format!("{}: {detail}", target.id));
            }
        };
        let found = sections.iter().find_map(|(_, addr, bytes)| {
            let at = line.addr.checked_sub(*addr)? as usize;
            bytes.get(at..at + line.len)
        });
        let Some(raw) = found else {
            fail(format!(
                "{:#010x}: no executable section holds these {} byte(s)",
                line.addr, line.len
            ));
            continue;
        };
        let mut bits = 0u32;
        for (i, b) in raw.iter().enumerate() {
            bits |= u32::from(*b) << (8 * i);
        }
        if bits != line.bits {
            fail(format!(
                "{:#010x}: the ELF holds {bits:0len$x}, objdump printed {:0len$x}",
                line.addr,
                line.bits,
                len = line.len * 2
            ));
            continue;
        }
        let Some(op) = decode_at(raw, line.addr) else {
            fail(format!("{:#010x}: {bits:08x} does not decode", line.addr));
            continue;
        };
        if usize::from(op.len) != line.len {
            fail(format!(
                "{:#010x}: decoded length {}, objdump {}",
                line.addr, op.len, line.len
            ));
            continue;
        }
        let got = format_op(&op, line.addr, bits);
        if got != line.text {
            fail(format!(
                "{:#010x}: decoded `{got}`, objdump `{}`",
                line.addr, line.text
            ));
        }
        counts.compared += 1;
    }
    Ok(counts)
}

/// Runs the decode corpus over `targets` and fails on any mismatch, coverage shortfall or
/// unreadable file. A missing objdump drops the objdump half with a printed reason.
fn run_decode_corpus(test: &str, targets: &[ElfTarget]) {
    let tool = objdump();
    let usable = Command::new(&tool)
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !usable {
        common::skip(
            test,
            "no riscv32-esp-elf-objdump (set PASSPORTSIM_OBJDUMP); the coverage half still runs",
        );
    }
    let tool = usable.then_some(tool.as_path());

    let mut problems = Vec::new();
    let mut results = Vec::new();
    for target in targets {
        match compare_one(target, tool) {
            Ok(counts) => {
                if counts.walked < target.min_insns {
                    problems.push(format!(
                        "{}: the coverage walk decoded {} instruction(s), at least {} expected",
                        target.id, counts.walked, target.min_insns
                    ));
                }
                if tool.is_some() && counts.compared < target.min_insns {
                    problems.push(format!(
                        "{}: objdump printed {} instruction line(s), at least {} expected",
                        target.id, counts.compared, target.min_insns
                    ));
                }
                results.push(counts);
            }
            Err(problem) => problems.push(format!("{}: {problem}", target.id)),
        }
    }

    let mut mismatches = 0;
    let mut compared = 0;
    let mut walked = 0;
    let mut details: Vec<&String> = Vec::new();
    for counts in &results {
        println!(
            "decode corpus {}: {} instruction(s) decoded, {} compared with objdump, {} mismatch(es), \
             {} data line(s) skipped",
            counts.id, counts.walked, counts.compared, counts.mismatches, counts.data
        );
        mismatches += counts.mismatches;
        compared += counts.compared;
        walked += counts.walked;
        details.extend(counts.details.iter());
    }
    assert!(
        problems.is_empty(),
        "decode corpus input problems: {problems:#?}"
    );
    assert_eq!(
        mismatches, 0,
        "decode corpus: {mismatches} of {compared} compared instruction(s) differ from objdump: {details:#?}"
    );
    assert!(
        !results.is_empty(),
        "the decode corpus compared no file at all, which never passes"
    );
    println!(
        "decode corpus: {walked} instruction(s) decoded over {} file(s), {compared} of them compared \
              with objdump",
        results.len()
    );
}

/// The two bundled ROM ELFs, with the bytes `pemu_loader::rom::bundled` embeds checked against the
/// files objdump reads, so the comparison is over the ROM every build runs.
fn bundled_rom_targets() -> Vec<ElfTarget> {
    RomRev::ALL
        .into_iter()
        .map(|rev| {
            let path = assets_rom().join(rev.file_name());
            let file = std::fs::read(&path).expect("the bundled ROM ELF is committed");
            if let Some(embedded) = rom::bundled_opt(rev) {
                assert_eq!(
                    embedded,
                    file.as_slice(),
                    "{}: the embedded ROM is not the file in assets/rom/",
                    rev.corpus_id()
                );
            }
            ElfTarget {
                id: rev.corpus_id().to_owned(),
                path,
                // 122734 instructions and more; the floor leaves room for another release of the same ROM.
                min_insns: 100_000,
            }
        })
        .collect()
}

/// Every instruction in the text sections of `rom3` and `rom101` decodes to what objdump prints.
#[test]
fn t0_m0_bundled_rom_elfs_decode_like_objdump() {
    let test = "t0_m0_bundled_rom_elfs_decode_like_objdump";
    run_decode_corpus(test, &bundled_rom_targets());
}

/// The same over `rom0` and the `pk`, `official` and `probe2` ELFs under the data root. An absent
/// file is skipped with a reason; a present one that is not the pinned one fails.
#[test]
fn t1_m0_corpus_elfs_decode_like_objdump() {
    let test = "t1_m0_corpus_elfs_decode_like_objdump";
    // (corpus id, file name, instruction floor): the pinned counts rounded down.
    let wanted: [(&str, &str, usize); 6] = [
        ("rom0", "esp32c3_rev0_rom.elf", 100_000),
        ("pk", "FoloToy-AI-Passport.elf", 100_000),
        ("pk", "bootloader.elf", 5_000),
        ("official", "FoloToy-AI-Passport.elf", 100_000),
        ("official", "bootloader.elf", 5_000),
        ("probe2", "radio_heapprobe.elf", 100_000),
    ];
    let mut targets = bundled_rom_targets();
    for (id, file, min_insns) in wanted {
        match common::corpus_file_or_skip(test, id, file) {
            Some(path) => targets.push(ElfTarget {
                id: format!("{id}/{file}"),
                path,
                min_insns,
            }),
            None => continue,
        }
    }
    run_decode_corpus(test, &targets);
}

// ------------------------------------------------------------------------------------------------
// The ESP32-C3 CSRs
// ------------------------------------------------------------------------------------------------
//
// The performance counters 0x7E0-0x7E2 and their user aliases 0x800-0x802, the counter pausing in
// WFI, CSR 0x000, PMP TOR with lock and vectored `mtvec` are asserted by running one program
// under `ref_step` and the engine at every block size. The FENCE encodings are asserted through
// the decoder over the whole reserved-field space and then executed in the same program.

/// `lui rd, imm20` (U format).
fn u_type(imm20: u32, rd: u32) -> u32 {
    (imm20 << 12) | (rd << 7) | 0x37
}

fn csr_read(csr: u16, rd: u32) -> u32 {
    i_type(u32::from(csr) as i32, 0, 2, rd, 0x73)
}

fn csr_write(csr: u16, rs1: u32) -> u32 {
    i_type(u32::from(csr) as i32, rs1, 1, 0, 0x73)
}

fn csr_write_imm(csr: u16, uimm: u32, rd: u32) -> u32 {
    i_type(u32::from(csr) as i32, uimm, 5, rd, 0x73)
}

fn csr_set(csr: u16, rs1: u32) -> u32 {
    i_type(u32::from(csr) as i32, rs1, 2, 0, 0x73)
}

/// The five locked TOR entries IDF's `esp_cpu_configure_region_protection` writes on the C3
/// (`esp_hw_support/port/esp32c3/cpu_region_protect.c`, `soc/soc.h` addresses), as
/// `(pmpaddr value, configuration byte)`: the NULL gap and DROM locked with no permission, the
/// debug region RWX, DROM R, DRAM RW. `PMP_L` is set on every one, so the entries are final.
const PMP_ENTRIES: [(u32, u32); 5] = [
    (0x2000_0000 >> 2, 0x88), // L | TOR, no permission: below the debug region
    (0x2800_0000 >> 2, 0x8F), // L | TOR | R | W | X: the debug region
    (0x3C00_0000 >> 2, 0x88), // L | TOR, no permission: the gap below DROM
    (0x3FC8_0000 >> 2, 0x89), // L | TOR | R: DROM
    (0x3FCE_0000 >> 2, 0x8B), // L | TOR | R | W: DRAM
];

/// A program that exercises every executable CSR clause once, in order, and the index of its
/// `wfi`, around which the pause claim reads the counter.
fn esp_csr_program() -> (Vec<u32>, usize) {
    // The ROM's `_init` enables the cycle counter through the user aliases.
    let mut p = vec![
        csr_write_imm(CSR_UPCER, 1, 0),
        csr_write_imm(CSR_UPCMR, 1, 0),
        csr_read(CSR_UPCCR, 5), // x5: the counter before the wait
    ];
    p.push(i_type(1, 0, 0, 6, 0x13));
    p.push(i_type(1, 6, 0, 6, 0x13));
    p.push(i_type(1, 6, 0, 6, 0x13));
    let wfi_at = p.len();
    p.push(0x1050_0073); // wfi
    p.push(csr_read(CSR_UPCCR, 7)); // x7: the counter after it
    p.push(csr_read(CSR_MPCCR, 8)); // x8: the machine number reads the same counter
    p.push(csr_read(CSR_MPCER, 9)); // x9: and so do the two control registers
    p.push(csr_read(CSR_MPCMR, 10));
    // CSR 0x000: the ROM writes it twice and never traps.
    p.push(csr_write_imm(CSR_USTATUS, 0, 0));
    p.push(csr_write_imm(CSR_USTATUS, 1, 11)); // x11: the old value, 0
    p.push(csr_read(CSR_USTATUS, 12)); // x12: 1
    p.push(i_type(0, 0, 0, 0, 0x0F)); // fence
    p.push(0x0000_100F); // fence.i
    // PMP TOR with lock, written as IDF writes it: `csrw` of pmpaddr, then `csrs` of the
    // configuration byte (`riscv/include/riscv/csr.h`).
    for (entry, (addr, cfg)) in PMP_ENTRIES.into_iter().enumerate() {
        p.push(u_type(addr >> 12, 13));
        p.push(csr_write(CSR_PMPADDR0 + entry as u16, 13));
        let bits = cfg << ((entry as u32 % 4) * 8);
        p.push(u_type((bits >> 12) + u32::from(bits & 0x800 != 0), 14));
        if bits & 0xFFF != 0 {
            p.push(i_type(((bits & 0xFFF) as i32) << 20 >> 20, 14, 0, 14, 0x13));
        }
        p.push(csr_set(CSR_PMPCFG0 + entry as u16 / 4, 14));
    }
    // A locked entry ignores a later write silently and still reads back the locked value
    // (`riscv/csr.h`). Without these instructions a CPU with no lock would pass.
    p.push(csr_write(CSR_PMPADDR0 + 3, 0)); // csrw pmpaddr3, x0: a write of 0
    p.push(csr_read(CSR_PMPADDR0 + 3, 18)); // x18: still the locked bound
    p.push(i_type(-1, 0, 0, 19, 0x13)); // x19 = all ones
    p.push(i_type(u32::from(CSR_PMPCFG0) as i32, 19, 3, 0, 0x73)); // csrrc x0, pmpcfg0, x19
    p.push(csr_read(CSR_PMPCFG0, 20)); // x20: still the locked bytes
    // Vectored `mtvec`: written with no mode bit and read back with it set, because the C3 has no
    // direct mode.
    p.push(u_type(VECTOR >> 12, 15));
    p.push(csr_write(CSR_MTVEC, 15));
    p.push(csr_read(CSR_MTVEC, 16)); // x16: VECTOR | 1
    // An exception enters at BASE even in vectored mode; the handler skips the `ecall`.
    p.push(0x0000_0073); // ecall
    p.push(i_type(0x5A, 0, 0, 17, 0x13)); // x17: proof the handler returned here
    p.push(j_type(0, 0)); // a self-loop
    (p, wfi_at)
}

/// Polls the bus until it reports a wake: neither `ref_step` nor the engine clears `Hart::wfi`,
/// because whether the wait is over is the SoC's answer (`Bus::wfi_wake` ignores `mstatus.MIE`).
/// Nothing here retires an instruction.
fn wait_for_wake(hart: &mut Hart, bus: &mut M0Bus) {
    assert!(hart.wfi, "only a waiting hart is woken");
    let waiting = hart.insns;
    while !bus.wfi_wake() {}
    assert_eq!(hart.insns, waiting, "waiting retired an instruction");
    hart.wfi = false;
}

/// Runs the CSR program under `ref_step`, or the engine when `max` is `Some`. `retire` exceeds the
/// program, so the run ends in the final self-loop.
fn esp_csr_run(program: &[u32], retire: u64, max: Option<u16>) -> (Hart, M0Bus) {
    let mut hart = hart(SpMonitor::default(), 0);
    let mut bus = M0Bus::new(program, SpMonitor::default()).with_cycle_counter();
    match max {
        None => {
            while hart.insns < retire {
                if let StepResult::Wfi = ref_step(&mut hart, &mut bus) {
                    wait_for_wake(&mut hart, &mut bus);
                }
            }
        }
        Some(max_block_insns) => {
            let mut engine = Engine::new(EngineCfg {
                max_block_insns,
                ..EngineCfg::default()
            });
            let hooks = HookSet::default();
            while hart.insns < retire {
                let budget = retire - hart.insns;
                match engine.run(&mut hart, &mut bus, &hooks, budget) {
                    Exit::Wfi => wait_for_wake(&mut hart, &mut bus),
                    Exit::Budget | Exit::Stop | Exit::SpSpill(_) => {}
                    other => panic!("the engine ended with {other:?}"),
                }
                for addr in std::mem::take(&mut bus.dirty) {
                    engine.invalidate_vrange(addr, 1);
                }
            }
        }
    }
    (hart, bus)
}

/// The ESP32-C3 CSRs beyond the standard machine-mode set behave as specified, in the reference
/// interpreter and in the engine at every block size.
#[test]
fn t0_m0_esp_csrs_behave_as_specified() {
    // --- FENCE. Every encoding is a NOP or a cache flush, whatever its reserved fields hold ---
    //
    // The whole 12-bit fm/pred/succ field crossed with the register fields, where a decoder that
    // checked a reserved field would trap (RISC-V unprivileged ISA, FENCE).
    let mut fences = 0;
    for upper in 0u32..4096 {
        for rs1 in [0u32, 1, 31] {
            for rd in [0u32, 1, 31] {
                for (funct3, want) in [(0u32, K_NOP), (1, K_FENCEI)] {
                    let insn = (upper << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | 0x0F;
                    let op = decode_at(&insn.to_le_bytes(), BASE).expect("a FENCE decodes");
                    assert_eq!(
                        op.kind, want,
                        "FENCE {insn:#010x} decoded to kind {} rather than {want}",
                        op.kind
                    );
                    assert_eq!(op.len, 4);
                    fences += 1;
                }
            }
        }
    }
    assert_eq!(fences, 4096 * 9 * 2, "every FENCE encoding was decoded");

    // --- The program, under both interpreters and every block size ---
    let (program, wfi_at) = esp_csr_program();
    let retire = program.len() as u64 + 16;
    let runs: Vec<(String, (Hart, M0Bus))> = core::iter::once(("ref_step".to_owned(), None))
        .chain(BLOCK_SIZES.map(|m| (format!("engine at {m}"), Some(m))))
        .map(|(name, max)| (name, esp_csr_run(&program, retire, max)))
        .collect();

    for (who, (hart, bus)) in &runs {
        let csr = &hart.csr;

        // Performance counters 0x7E0-0x7E2 and the user aliases 0x800-0x802.
        assert_eq!(
            (csr.mpcer, csr.mpcmr),
            (MPCER_CYCLE, MPCMR_COUNT_EN),
            "{who}: writing the user aliases 0x800 and 0x801 lands in the machine registers"
        );
        assert_eq!(
            hart.x[9], MPCER_CYCLE,
            "{who}: 0x7E0 reads back what 0x800 wrote"
        );
        assert_eq!(
            hart.x[10], MPCMR_COUNT_EN,
            "{who}: 0x7E1 reads back what 0x801 wrote"
        );
        assert_eq!(
            hart.x[8],
            hart.x[7] + 1,
            "{who}: 0x7E2 and 0x802 are one counter, and the machine number is read one \
             instruction after the user one",
        );

        // The counter pauses in WFI (TRM Register 1.12): between the readings at program indices 2 and
        // `wfi_at + 1` the hart retired exactly the instructions between them, however often it polled.
        let retired_between = (wfi_at + 1 - 2) as u32;
        assert_eq!(
            hart.x[7] - hart.x[5],
            retired_between,
            "{who}: the cycle counter counted something other than the {retired_between} \
             instructions that retired between the two reads"
        );
        assert!(
            bus.wfi_polls > 0,
            "{who}: the hart never waited, so the pause claim would be vacuous"
        );

        // CSR 0x000 is a plain register and never reaches the bus.
        assert_eq!(
            hart.x[11], 0,
            "{who}: the first write to CSR 0x000 read back 0"
        );
        assert_eq!(hart.x[12], 1, "{who}: CSR 0x000 kept the second write");
        assert_eq!(csr.csr000, 1, "{who}: and kept it in its own field");
        // `MSTATUS_WRITE_MASK` covers bit 0, so an alias of CSR 0x000 would show in `mstatus`.
        assert_eq!(
            csr.mstatus & 1,
            0,
            "{who}: CSR 0x000 is not aliased onto mstatus"
        );

        // PMP TOR with lock: the five entries IDF writes, read back.
        assert_eq!(
            u32::from_le_bytes(csr.pmpcfg[0..4].try_into().unwrap()),
            0x8988_8F88,
            "{who}: pmpcfg0"
        );
        assert_eq!(
            u32::from_le_bytes(csr.pmpcfg[4..8].try_into().unwrap()),
            0x0000_008B,
            "{who}: pmpcfg1"
        );
        for (entry, (addr, cfg)) in PMP_ENTRIES.into_iter().enumerate() {
            assert_eq!(csr.pmpaddr[entry], addr, "{who}: pmpaddr{entry}");
            assert_eq!(
                u32::from(csr.pmpcfg[entry]),
                cfg,
                "{who}: pmpcfg byte {entry}"
            );
            assert_eq!(
                csr.pmpcfg[entry] & (PMP_L | PMP_TOR),
                PMP_L | PMP_TOR,
                "{who}: entry {entry} is a locked TOR entry"
            );
        }
        assert_eq!(
            hart.x[18], PMP_ENTRIES[3].0,
            "{who}: a locked pmpaddr ignored a write and read back its own value"
        );
        assert_eq!(
            hart.x[20], 0x8988_8F88,
            "{who}: locked pmpcfg bytes ignored a clear of every bit"
        );
        let pmp = Pmp::from_csr(csr);
        let m = Privilege::Machine;
        for (addr, kind, allowed, what) in [
            (
                0x0000_1000u32,
                AccessKind::Read,
                false,
                "the NULL gap is locked with no permission",
            ),
            (
                0x2000_0000,
                AccessKind::Execute,
                true,
                "the debug region is RWX",
            ),
            (0x3C00_0000, AccessKind::Read, true, "DROM is readable"),
            (
                0x3C00_0000,
                AccessKind::Write,
                false,
                "DROM is not writable",
            ),
            (0x3FC8_1000, AccessKind::Write, true, "DRAM is writable"),
            (
                0x3FC8_1000,
                AccessKind::Execute,
                false,
                "DRAM is not executable",
            ),
            (
                0x6000_0000,
                AccessKind::Write,
                true,
                "no entry matches, so machine mode passes",
            ),
        ] {
            assert_eq!(pmp.check(addr, 4, kind, m), allowed, "{who}: {what}");
        }

        // The mode bit comes back set, and the `ecall` entered at BASE, not BASE + 4 * cause.
        assert_eq!(hart.x[16], VECTOR | 1, "{who}: mtvec forces vectored mode");
        assert_eq!(csr.mtvec, VECTOR | 1, "{who}: and stores it that way");
        assert_eq!(csr.mcause, 11, "{who}: the ecall raised machine-mode ECALL");
        assert_eq!(
            hart.x[17], 0x5A,
            "{who}: the handler at BASE returned to the instruction after the ecall"
        );
    }

    // Every interpreter produced the same registers and counter.
    let (first_name, (first_hart, _)) = &runs[0];
    for (who, (hart, _)) in &runs[1..] {
        assert_eq!(hart.x, first_hart.x, "{who} disagrees with {first_name}");
        assert_eq!(
            hart.insns, first_hart.insns,
            "{who} retired a different count"
        );
    }

    // Every access to a time-derived CSR reached the bus, so the SoC can date it. Checked on the
    // reference run, since the engine may re-enter a block.
    let (_, (_, bus)) = &runs[0];
    let seen: Vec<(u16, CsrOp)> = bus.clock_calls.iter().map(|&(n, op, _)| (n, op)).collect();
    assert_eq!(
        seen,
        [
            (CSR_UPCER, CsrOp::Write(1)),
            (CSR_UPCMR, CsrOp::Write(1)),
            (CSR_UPCCR, CsrOp::Read),
            (CSR_UPCCR, CsrOp::Read),
            (CSR_MPCCR, CsrOp::Read),
            (CSR_MPCER, CsrOp::Read),
            (CSR_MPCMR, CsrOp::Read),
        ],
        "every 0x7E0-0x7E2 and 0x800-0x802 access reaches Bus::csr_custom, in program order"
    );

    // The wake-up interrupt retires no instruction either, and enters at BASE + 4 * line (IDF
    // `riscv/vectors_intc.S`).
    let (mut hart, mut bus) = esp_csr_run(&program, retire, None);
    hart.csr.mstatus = MSTATUS_MIE;
    hart.wfi = true;
    let waiting = hart.insns;
    let before = hart
        .csr
        .read(&mut bus, csr_cx(hart.insns), CSR_MPCCR)
        .unwrap();
    trap::take_interrupt(&mut hart, 7);
    assert!(!hart.wfi, "the interrupt ended the wait");
    assert_eq!(
        hart.insns, waiting,
        "the wake-up interrupt retires no instruction"
    );
    assert_eq!(
        hart.pc,
        VECTOR + 4 * 7,
        "interrupt 7 enters at BASE + 4 * 7"
    );
    assert_eq!(hart.csr.mcause, 0x8000_0007);
    let after = hart
        .csr
        .read(&mut bus, csr_cx(hart.insns), CSR_MPCCR)
        .unwrap();
    assert_eq!(
        after, before,
        "and the cycle counter did not move across the wait"
    );
}

fn csr_cx(insns: u64) -> pemu_rv32::csr::CsrCx {
    pemu_rv32::csr::CsrCx {
        insns,
        insn: csr_read(CSR_MPCCR, 10),
        strict: false,
    }
}

// ------------------------------------------------------------------------------------------------
// The scheduler, the clock formulas and the snapshot codec
// ------------------------------------------------------------------------------------------------

fn key(tag: u16) -> EventKey {
    EventKey {
        owner: Owner::Periph(PeriphId(tag / 4)),
        tag,
    }
}

/// The scheduler delivers in (time, seq) order and cancels in place.
///
/// Over seeded random schedules, against a list the test keeps: the events handed out are exactly
/// the live ones, in `(time, seq)` order. `seq` is insertion order, so ties are ordered by
/// scheduling and nothing else.
#[test]
fn t0_m0_scheduler_orders_by_time_then_seq_and_cancels_in_place() {
    for round in 0..64u64 {
        let mut rng = DetRng::new(0x4D30_5F45_302E_3800 ^ round);
        let mut draw = || rng.stream(RngStream(0)).next_u32();
        let mut sched = Scheduler::new();
        let mut live: Vec<(u64, u64, EventKey)> = Vec::new();
        let mut handles: Vec<(EventHandle, u64, u64, EventKey)> = Vec::new();
        let mut seq = 0u64;
        let now = VTime(0);

        for step in 0..64u32 {
            if step % 5 == 4 && !handles.is_empty() {
                // Cancel one event twice, then a stale handle: none may disturb the rest.
                let victim = (draw() as usize) % handles.len();
                let (handle, _, gone_seq, _) = handles.remove(victim);
                assert!(sched.is_pending(handle), "the victim was still pending");
                sched.cancel(handle);
                assert!(!sched.is_pending(handle), "cancel unscheduled the event");
                assert_eq!(sched.time_of(handle), None);
                sched.cancel(handle);
                live.retain(|&(_, s, _)| s != gone_seq);
            } else {
                // The times repeat on purpose: ties are what `seq` exists for.
                let at = VTime(u64::from(draw() % 8) * 1_000_000);
                let k = key((draw() % 64) as u16);
                let handle = sched.schedule(now, at, k);
                assert_eq!(sched.time_of(handle), Some(at));
                live.push((at.0, seq, k));
                handles.push((handle, at.0, seq, k));
                seq += 1;
            }
            assert_eq!(sched.len(), live.len(), "round {round} step {step}");
            assert_eq!(sched.is_empty(), live.is_empty());
            let earliest = live.iter().map(|&(t, _, _)| t).min().map(VTime);
            assert_eq!(sched.next_time(), earliest, "round {round} step {step}");
        }

        // `pending()` is the drain order, which is what `inspect sched` shows.
        let mut want = live.clone();
        want.sort_unstable_by_key(|&(t, s, _)| (t, s));
        assert_eq!(
            sched.pending(),
            want.iter()
                .map(|&(t, s, k)| (VTime(t), s, k))
                .collect::<Vec<_>>(),
            "round {round}: pending() is (time, seq) order"
        );

        let mut drained = Vec::new();
        for &(t, _, _) in &want {
            let at = VTime(t);
            if let Some(before) = t.checked_sub(1) {
                assert_eq!(
                    sched.pop_due(VTime(before)),
                    None,
                    "round {round}: an event came out {} ps early",
                    at.0 - before
                );
            }
            drained.push(sched.pop_due(at).expect("the event at its own time"));
        }
        assert_eq!(
            drained,
            want.iter().map(|&(_, _, k)| k).collect::<Vec<_>>(),
            "round {round}: delivery order is (time, seq)"
        );
        assert!(sched.is_empty(), "round {round}: the drain emptied it");
        assert_eq!(sched.pop_due(VTime(u64::MAX)), None);

        // A fired event's handle cancels nothing, even once its slot holds a newer event.
        let stale = handles.first().map(|&(h, _, _, _)| h);
        if let Some(stale) = stale {
            let fresh = sched.schedule(now, VTime(7), key(1));
            sched.cancel(stale);
            assert!(
                sched.is_pending(fresh),
                "round {round}: a stale handle cancelled somebody else's event"
            );
            assert_eq!(sched.len(), 1);
        }
    }

    // `at` before `now` is clamped to `now`.
    let mut sched = Scheduler::new();
    let late = sched.schedule(VTime(500), VTime(100), key(9));
    assert_eq!(sched.time_of(late), Some(VTime(500)));
    assert_eq!(sched.next_time(), Some(VTime(500)));
    assert_eq!(sched.pop_due(VTime(499)), None);
    assert_eq!(sched.pop_due(VTime(500)), Some(key(9)));
}

/// The clock formulas hold: virtual time, the CPU cycle counter, the SYSTIMER tick and the audio
/// frame clock, each against the arithmetic, after sequences of rebases, stalls and idle jumps
/// where an implementation keeping a rounded base would drift.
#[test]
fn t0_m0_clock_formulas_hold_for_time_cycles_and_systimer() {
    // The default profile: 160 MHz, CPI 1, 6250 ps per instruction.
    assert_eq!(ps_per_cycle(DEFAULT_CPU_HZ), 6_250);
    let clock = Clock::new(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
    assert_eq!(clock.ps_per_insn(), 6_250);
    assert_eq!(clock.now(1_000_000), VTime(6_250_000_000));
    assert!(!clock.counting(), "the counter is disabled out of reset");
    assert_eq!(clock.cycle_count(1_000_000), 0);

    // SYSTIMER counts 16 ticks per microsecond whatever the CPU is doing (TRM System Timer
    // chapter).
    assert_eq!(SYSTIMER_TICK_PS, 62_500);
    assert_eq!(VTime::from_us(1).0 / SYSTIMER_TICK_PS, 16);
    let epoch = VTime::from_us(7);
    assert_eq!(systimer_count(epoch, epoch, 0), 0);
    assert_eq!(
        systimer_count(VTime(epoch.0 - 1), epoch, 5),
        5,
        "before the epoch reads the load"
    );
    for us in [0u64, 1, 1_000, 1_000_000] {
        let now = VTime(epoch.0 + us * 1_000_000);
        assert_eq!(
            systimer_count(now, epoch, 0),
            us * 16,
            "{us} us is {} ticks",
            us * 16
        );
        // One tick short of the next tick still reads the same count: nothing interpolates.
        let just_before = VTime(now.0 + SYSTIMER_TICK_PS - 1);
        assert_eq!(systimer_count(just_before, epoch, 0), us * 16);
        assert_eq!(
            systimer_deadline(epoch, us * 16),
            now,
            "the deadline inverts the count"
        );
    }
    assert_eq!(
        systimer_count(VTime(u64::MAX), VTime(0), SYSTIMER_COUNTER_MASK),
        (u64::MAX / SYSTIMER_TICK_PS).wrapping_add(SYSTIMER_COUNTER_MASK) & SYSTIMER_COUNTER_MASK,
        "the counter is 52 bits wide and wraps there"
    );

    // Frame n at fs starts at start + n * 1e12 / fs, in u128, so 44.1 kHz does not drift.
    let start = VTime::from_ms(3);
    for fs in [8_000u32, 16_000, 44_100, 48_000] {
        assert_eq!(frame_time(start, 0, fs), start);
        for n in [1u64, 441, 100_000, 10_000_000] {
            let want = start.0 + (u128::from(n) * 1_000_000_000_000 / u128::from(fs)) as u64;
            assert_eq!(frame_time(start, n, fs).0, want, "frame {n} at {fs} Hz");
        }
    }
    assert_eq!(
        frame_time(start, 7, 0),
        start,
        "an unconfigured I2S clock does not divide by 0"
    );

    for round in 0..64u64 {
        let mut rng = DetRng::new(0x4D30_5F45_302E_3840 ^ round);
        let mut draw = || rng.stream(RngStream(0)).next_u32();
        let hz = [80_000_000u32, 160_000_000, 40_000_000, 17_500_000];
        let mut clock = Clock::new(hz[(draw() % 4) as usize], 1_000 + draw() % 3_000);
        clock.set_counting(0, true);
        let mut insns = 0u64;
        // `folded` is the cycle total already folded into the base, in 10^-12 cycles, and `segment_ps`
        // the executed picoseconds since. The counter is `(folded + segment_ps * cpu_hz) / 10^12`;
        // keeping the remainder makes a frequency change or a stall cost nothing in accuracy.
        const PS_PER_S: u128 = 1_000_000_000_000;
        let mut folded = 0u128;
        let mut segment_ps = 0u128;
        let mut cpu_hz = u128::from(clock.cpu_hz());
        let mut time = 0u64;

        for step in 0..48u32 {
            let run = u64::from(draw() % 1_000 + 1);
            insns += run;
            let d = run * clock.ps_per_insn();
            time += d;
            segment_ps += u128::from(d);
            assert_eq!(
                clock.now(insns),
                VTime(time),
                "round {round} step {step}: now"
            );
            assert_eq!(
                u128::from(clock.cycle_count(insns)),
                (folded + segment_ps * cpu_hz) / PS_PER_S,
                "round {round} step {step}: cycle_count"
            );
            match draw() % 4 {
                0 => {
                    // A frequency change: both readings stay continuous at this instruction.
                    let before = (clock.now(insns), clock.cycle_count(insns));
                    let next = hz[(draw() % 4) as usize];
                    clock.rebase(insns, next);
                    assert_eq!((clock.now(insns), clock.cycle_count(insns)), before);
                    folded += segment_ps * cpu_hz;
                    segment_ps = 0;
                    cpu_hz = u128::from(next);
                }
                1 => {
                    // A stall credits time; it credits cycles only when the profile says so.
                    let ps = u64::from(draw() % 100_000);
                    let counts = draw() % 2 == 0;
                    let cc = clock.cycle_count(insns);
                    clock.stall(insns, ps, counts);
                    time += ps;
                    assert_eq!(clock.now(insns), VTime(time));
                    if counts {
                        folded += u128::from(ps) * cpu_hz;
                    } else {
                        assert_eq!(clock.cycle_count(insns), cc, "a free stall counts no cycle");
                    }
                }
                2 => {
                    // Idle: time jumps, instructions and cycles do not.
                    let cc = clock.cycle_count(insns);
                    let to = VTime(time + u64::from(draw()) + 1);
                    clock.idle_until(insns, to);
                    time = to.0;
                    assert_eq!(clock.now(insns), to);
                    assert_eq!(clock.cycle_count(insns), cc, "WFI counts no cycle");
                    clock.idle_until(insns, VTime(time - 1));
                    assert_eq!(clock.now(insns), to);
                }
                _ => {}
            }
            // `insns_until` is the smallest n >= 1 that reaches `t`.
            let ahead = VTime(time + u64::from(draw() % 1_000_000));
            let n = clock.insns_until(insns, ahead);
            assert!(n >= 1);
            assert!(
                clock.now(insns + n) >= ahead,
                "round {round} step {step}: insns_until"
            );
            if n > 1 {
                assert!(clock.now(insns + n - 1) < ahead, "insns_until is not ceil");
            }
            assert_eq!(
                clock.insns_until(insns, VTime(time)),
                1,
                "now itself is one instruction"
            );
        }
        assert!(clock.idle_ps() > 0 || clock.stall_ps() > 0 || insns > 0);
    }
}

use pemu_loader::hex;

/// The `sched` section of a scheduler with three pending events, one cancelled slot and a fourth
/// event reusing it, built by real calls so the bytes describe a reachable state.
fn sample_scheduler() -> Scheduler {
    let mut sched = Scheduler::new();
    let now = VTime(0);
    sched.schedule(now, VTime::from_us(5), key(1));
    let cancelled = sched.schedule(now, VTime::from_us(2), key(2));
    sched.schedule(now, VTime::from_us(5), key(3));
    sched.cancel(cancelled);
    sched.schedule(now, VTime::from_us(9), key(4));
    sched
}

/// The `rng` section of a `DetRng` three streams into a run: the seed and the per-stream
/// positions, never the key or the keystream cache. The seed is made up, not a device value.
fn sample_rng() -> DetRng {
    let mut rng = DetRng::new(0x4D30_5F45_302E_3800);
    rng.stream(RngStream::GUEST_ENTROPY).next_u64();
    rng.stream(RngStream::BOARD_NOISE).next_u32();
    let mut out = [0u8; 40];
    rng.stream(RngStream::IDENTITY).fill_bytes(&mut out);
    rng
}

/// `sched` v1 of [`sample_scheduler`], as postcard writes it.
const SCHED_GOLDEN_V1: &str = "04030001c096b102000000010101c0a8a504030001040001c096b1020200000300";

/// `rng` v1 of [`sample_rng`], as postcard writes it.
const RNG_GOLDEN_V1: &str = "80f0b881d3e897984d0300020101020a";

/// The snapshot codec writes the same bytes for the `sched` and `rng` sections as when they were
/// pinned. A round trip cannot see a renamed or reordered field, or a changed integer encoding.
#[test]
fn t0_m0_snapshot_sections_keep_their_golden_bytes() {
    let sched = sample_scheduler();
    let section = sched.encode().expect("the sched section encodes");
    assert_eq!(Scheduler::section_id(), SectionId::new(SectionId::SCHED));
    assert_eq!((section.version, section.codec), (1, Codec::Postcard));
    assert_eq!(hex(&section.bytes), SCHED_GOLDEN_V1, "sched v1");

    let rng = sample_rng();
    let section = rng.encode().expect("the rng section encodes");
    assert_eq!(DetRng::section_id(), SectionId::new(SectionId::RNG));
    assert_eq!((section.version, section.codec), (1, Codec::Postcard));
    assert_eq!(hex(&section.bytes), RNG_GOLDEN_V1, "rng v1");

    // Restoring the bytes gives back the same events in the same order and the same positions.
    let restored = Scheduler::decode(&sample_scheduler().encode().unwrap()).expect("sched decodes");
    assert_eq!(restored.len(), sample_scheduler().len());
    assert_eq!(restored.pending(), sample_scheduler().pending());
    let mut restored = DetRng::decode(&sample_rng().encode().unwrap()).expect("rng decodes");
    let mut original = sample_rng();
    assert_eq!(restored.seed(), original.seed());
    assert_eq!(
        restored.positions().collect::<Vec<_>>(),
        original.positions().collect::<Vec<_>>()
    );
    for id in [
        RngStream::GUEST_ENTROPY,
        RngStream::BOARD_NOISE,
        RngStream::IDENTITY,
    ] {
        assert_eq!(
            restored.stream(id).next_u64(),
            original.stream(id).next_u64(),
            "stream {id:?} carries on where the snapshot left it"
        );
    }

    // A section at another version is refused rather than read as this one.
    let mut wrong = sample_scheduler().encode().unwrap();
    wrong.version = 2;
    assert!(
        Scheduler::decode(&wrong).is_err(),
        "a version bump needs a migration"
    );
}

/// `doctor` in process over an empty `HOME`: the discovery, the host facts the CLI adds, and the
/// verdict, from an injected environment so nothing of this host leaks in.
#[test]
fn t0_m0_doctor_reports_roms_host_and_roles_from_an_empty_home() {
    use pemu_host::assets::{HostEnv, RomOptions, discovery_report, host_facts, resolve_rom};
    use pemu_host::paths::{Env, HostPaths, Overrides};
    use pemu_loader::rom::RomRev;

    let home = std::env::temp_dir().join(format!("pemu-doctor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).expect("an empty HOME");
    // On Windows the roles are relocated by `PASSPORTSIM_HOME` over this host's real known folders
    // (the resolver reads no `USERPROFILE`, `APPDATA` or `LOCALAPPDATA`).
    #[cfg(windows)]
    let paths = HostPaths::new(Env::windows(
        pemu_host::paths::known_folders().expect("this host's known folders"),
        Overrides {
            home: Some(home.to_str().expect("a UTF-8 temp path").to_owned()),
            ..Overrides::default()
        },
    ));
    #[cfg(not(windows))]
    let paths = HostPaths::new(Env::macos(&home, Overrides::default()));
    let env = HostEnv {
        home: Some(home.clone()),
        config_dir: paths.config().expect("the config role"),
        espressif_dir: None,
        idf_tools_path: None,
        rom_env: None,
        corpus_env: Vec::new(),
        data_root: paths.data_root().ok(),
    };

    let mut report =
        discovery_report(&env, &RomOptions::default(), RomRev::Rev101, true).to_doctor_report();
    report.host = Some(host_facts(&paths));

    let revs: Vec<&str> = report.bundled_roms.iter().map(|r| r.rev.as_str()).collect();
    assert_eq!(revs, ["rom101", "rom3"]);
    for rom in &report.bundled_roms {
        assert_eq!(
            rom.embedded_sha256.as_deref(),
            Some(rom.pinned_sha256.as_str()),
            "{rom:?}"
        );
    }
    assert!(report.rom_override.is_none());

    // The triple names this build's architecture and OS, and every role resolves below the injected
    // HOME.
    let host = report.host.as_ref().expect("the host facts");
    assert!(
        host.target.starts_with(std::env::consts::ARCH),
        "{}",
        host.target
    );
    assert!(
        host.target.contains(if cfg!(target_os = "macos") {
            "apple-darwin"
        } else {
            "windows"
        }),
        "{}",
        host.target
    );
    let os = host
        .os_version
        .as_ref()
        .expect("the OS version is readable on both hosts");
    let name = if cfg!(target_os = "macos") {
        "macOS "
    } else {
        "Windows "
    };
    assert!(os.starts_with(name), "{os}");
    let roles: Vec<&str> = host.roles.iter().map(|r| r.role.as_str()).collect();
    assert_eq!(
        roles,
        [
            "home",
            "config",
            "data",
            "cache",
            "run",
            "logs",
            "artifacts"
        ]
    );
    for role in &host.roles {
        let path = role.path.as_ref().expect("every role resolves");
        assert!(
            std::path::Path::new(path).starts_with(&home),
            "{} -> {path}",
            role.role
        );
    }

    let out = pemu_api::commands::doctor::run_report(&report).expect("a clean home is healthy");
    assert!(
        out.text.contains(&format!("host: {}", host.target)),
        "{}",
        out.text
    );
    assert!(out.text.contains("directory roles\n"), "{}", out.text);
    assert_eq!(out.json["host"]["roles"].as_array().map(Vec::len), Some(7));

    // The refusals: an override naming nothing, and one naming bytes no pin knows.
    let missing = HostEnv {
        rom_env: Some("/nonexistent".to_owned()),
        ..env.clone()
    };
    let err = resolve_rom(&missing, &RomOptions::default(), RomRev::Rev101).expect_err("refused");
    assert_eq!(err.code_name(), "E_ASSET_MISSING");
    let unpinned = home.join("rom0.elf");
    std::fs::write(&unpinned, b"not a pinned ROM").expect("a file");
    let wrong = HostEnv {
        rom_env: Some(unpinned.display().to_string()),
        ..env
    };
    let err = resolve_rom(&wrong, &RomOptions::default(), RomRev::Rev101).expect_err("refused");
    assert_eq!(err.code_name(), "E_ASSET_HASH");

    let _ = std::fs::remove_dir_all(&home);
}
