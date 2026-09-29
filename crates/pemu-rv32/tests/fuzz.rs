//! Fuzz: the block engine at `max_block_insns` 1, 3 and 64 must equal the reference stepper on
//! every register, CSR, counter and memory byte after the same retired count, over random
//! programs and memory maps drawn from a seeded [`DetRng`] (the printed seed reproduces a failure).
//! Every machine's trap handler skips the faulting instruction, so coverage continues past a trap.
//! `PEMU_FUZZ_ITERS` sets the iteration count (default 10^4) and `PEMU_FUZZ_SEED` the base seed.

// `clippy.toml` bans `std::env` in core crates; test code may read the sizing variables.
#![allow(clippy::disallowed_methods)]

use pemu_core::rng::{DetRng, RngStream};
use pemu_rv32::bus::{
    Access, Bus, CodePage, HartView, PF_CODE, PF_MMIO, PF_R, PF_SLOW, PF_W, PF_X, PageTable,
};
use pemu_rv32::cost::InsnCosts;
use pemu_rv32::csr::{Csr, CsrEffect, CsrOp};
use pemu_rv32::engine::{Engine, EngineCfg, Exit, HookSet};
use pemu_rv32::exec::Hart;
use pemu_rv32::refstep::{StepResult, ref_step_costed};
use pemu_rv32::spmon::SpMonitor;
use pemu_rv32::trap::Trap;

use std::collections::BTreeMap;

const DEFAULT_ITERATIONS: u32 = 10_000;

/// Each iteration derives its own seed from this, so one iteration replays on its own.
const DEFAULT_SEED: u64 = 0x5041_5353_504F_5254;

const PAGE: u32 = 4096;

const PAGES: u32 = 6;

/// Pages the pc may reach (program, padding, trap handler). They carry `PF_CODE`, so stores there
/// take the slow path and invalidate; the data pages above exercise the inlined fast store.
const CODE_PAGES: u32 = 4;

/// SRAM1 through the instruction bus.
const BASE: u32 = 0x4038_0000;

const VECTOR: u32 = BASE + 3 * PAGE;

const MAX_PROGRAM_INSNS: u32 = 256;

const MAX_RETIRED: u64 = 160;

/// Caps so a program that stops on every instruction cannot hang the test.
const MAX_RUN_CALLS: u32 = 4_000;

const MAX_STEPS: u32 = 8_000;

const BLOCK_SIZES: [u16; 3] = [1, 3, 64];

struct Gen {
    rng: DetRng,
}

impl Gen {
    fn new(seed: u64) -> Gen {
        Gen {
            rng: DetRng::new(seed),
        }
    }

    fn u32(&mut self) -> u32 {
        self.rng.stream(RngStream(0)).next_u32()
    }

    fn below(&mut self, n: u32) -> u32 {
        self.u32() % n
    }

    fn chance(&mut self, percent: u32) -> bool {
        self.below(100) < percent
    }

    /// Biased towards x0..x7, which hold pointers into the image, so memory accesses mostly hit.
    fn reg(&mut self) -> u32 {
        if self.chance(40) {
            self.below(8)
        } else {
            self.below(32)
        }
    }
}

// Instruction encoders (RISC-V unprivileged specification formats).

fn r_type(funct7: u32, rs2: u32, rs1: u32, funct3: u32, rd: u32, op: u32) -> u32 {
    (funct7 << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | op
}

fn i_type(imm: i32, rs1: u32, funct3: u32, rd: u32, op: u32) -> u32 {
    (((imm as u32) & 0xFFF) << 20) | (rs1 << 15) | (funct3 << 12) | (rd << 7) | op
}

fn s_type(imm: i32, rs2: u32, rs1: u32, funct3: u32, op: u32) -> u32 {
    let imm = (imm as u32) & 0xFFF;
    ((imm >> 5) << 25) | (rs2 << 20) | (rs1 << 15) | (funct3 << 12) | ((imm & 31) << 7) | op
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

const MRET: u32 = 0x3020_0073;
const CSR_MEPC: u32 = 0x341;

/// `mepc += 4; mret`: a trap skips the faulting instruction instead of looping on it.
fn trap_handler() -> [u32; 4] {
    [
        i_type(CSR_MEPC as i32, 0, 2, 30, 0x73), // csrrs x30, mepc, x0
        i_type(4, 30, 0, 30, 0x13),              // addi x30, x30, 4
        i_type(CSR_MEPC as i32, 30, 1, 0, 0x73), // csrrw x0, mepc, x30
        MRET,
    ]
}

/// One random instruction word and its length in bytes. The mix covers every class the engine
/// treats differently: in-block computation, memory, block terminators and raw halfwords.
fn instruction(g: &mut Gen, pc_index: u32, insns: u32) -> (u32, u32) {
    let target = |g: &mut Gen, pc_index: u32, insns: u32| -> i32 {
        let to = g.below(insns.max(1)) as i32;
        (to - pc_index as i32) * 4
    };
    match g.below(100) {
        0..=37 => {
            let rd = g.reg();
            let rs1 = g.reg();
            let rs2 = g.reg();
            let imm = (g.u32() % 4096) as i32 - 2048;
            let word = match g.below(10) {
                0 => i_type(imm, rs1, 0, rd, 0x13),       // addi
                1 => i_type(imm, rs1, 2, rd, 0x13),       // slti
                2 => i_type(imm, rs1, 4, rd, 0x13),       // xori
                3 => i_type(imm & 31, rs1, 1, rd, 0x13),  // slli
                4 => r_type(0, rs2, rs1, 0, rd, 0x33),    // add
                5 => r_type(0x20, rs2, rs1, 0, rd, 0x33), // sub
                6 => r_type(0x20, rs2, rs1, 5, rd, 0x33), // sra
                7 => r_type(1, rs2, rs1, 0, rd, 0x33),    // mul
                8 => r_type(1, rs2, rs1, 4, rd, 0x33),    // div
                _ => r_type(1, rs2, rs1, 6, rd, 0x33),    // rem
            };
            (word, 4)
        }
        38..=42 => ((g.u32() & !0xFFF) | (g.reg() << 7) | 0x37, 4), // lui
        43..=45 => ((g.u32() & !0xFFF) | (g.reg() << 7) | 0x17, 4), // auipc
        46..=59 => {
            let funct3 = [0u32, 1, 2, 4, 5][g.below(5) as usize];
            let imm = (g.u32() % 512) as i32 - 256;
            (i_type(imm, g.reg(), funct3, g.reg(), 0x03), 4)
        }
        60..=73 => {
            let funct3 = g.below(3);
            let imm = (g.u32() % 512) as i32 - 256;
            (s_type(imm, g.reg(), g.reg(), funct3, 0x23), 4)
        }
        74..=83 => {
            let funct3 = [0u32, 1, 4, 5, 6, 7][g.below(6) as usize];
            (
                b_type(target(g, pc_index, insns), g.reg(), g.reg(), funct3),
                4,
            )
        }
        84..=87 => (j_type(target(g, pc_index, insns), g.reg()), 4),
        88..=89 => (i_type((g.u32() % 64) as i32, g.reg(), 0, g.reg(), 0x67), 4), // jalr
        90..=93 => {
            // Includes CSRs the C3 lacks, to cover `Bus::csr_custom`.
            let csrs = [
                0x300u32, 0x305, 0x340, 0x341, 0x342, 0x343, 0x3A0, 0x3B0, 0x7A0, 0x7A1, 0x7A2,
                0x7E0, 0x7E1, 0x7E2, 0x800, 0x801, 0x802, 0x000, 0xF14, 0x180,
            ];
            let csr = csrs[g.below(csrs.len() as u32) as usize];
            let funct3 = 1 + g.below(6);
            (i_type(csr as i32, g.reg(), funct3, g.reg(), 0x73), 4)
        }
        94 => (0x0000_0073, 4), // ecall
        95 => (0x0010_0073, 4), // ebreak
        96 => (MRET, 4),
        97 => (0x1050_0073, 4), // wfi
        98 => (0x0000_100F, 4), // fence.i
        // Compressed, hint and illegal encodings in their natural proportions.
        _ => (g.u32() & 0xFFFF, 2),
    }
}

#[derive(Clone)]
struct Plan {
    image: Vec<u8>,
    /// Zero means absent from the page table: the slow path still serves the page.
    flags: [u32; PAGES as usize],
    /// Starting offset; sometimes just short of a page boundary so an instruction straddles it.
    entry: u32,
    /// Stores to this page answer `Access::OkStop`, as an MMIO write does.
    stop_page: Option<u32>,
    /// The bus refuses this page, so accesses fault.
    dead_page: Option<u32>,
    spmon: SpMonitor,
    x: [u32; 32],
    mstatus: u32,
    retire: u64,
    /// Zero in the uncosted leg.
    costs: InsnCosts,
    /// Seed for cutting engine runs into random budgets; `None` runs the whole budget left.
    chop: Option<u64>,
}

fn plan(g: &mut Gen) -> Plan {
    let insns = 1 + g.below(MAX_PROGRAM_INSNS);
    let mut image = vec![0u8; (PAGES * PAGE) as usize];
    // One run in four starts just short of the page boundary, reaching a straddling instruction,
    // a block indexed under two pages and a fault on the second halfword alone. All branches are
    // pc-relative, so the program moves as a whole.
    let entry = if g.chance(25) {
        PAGE - 2 * g.below(5)
    } else {
        0
    };
    let mut at = entry as usize;
    for i in 0..insns {
        let (word, len) = instruction(g, i, insns);
        let bytes = word.to_le_bytes();
        if at + len as usize > (2 * PAGE) as usize {
            break;
        }
        image[at..at + len as usize].copy_from_slice(&bytes[..len as usize]);
        at += len as usize;
    }
    let mut at = (VECTOR - BASE) as usize;
    for word in trap_handler() {
        image[at..at + 4].copy_from_slice(&word.to_le_bytes());
        at += 4;
    }

    let mut flags = [0u32; PAGES as usize];
    for (p, f) in flags.iter_mut().enumerate() {
        *f = if (p as u32) < CODE_PAGES {
            let mut f = PF_R | PF_X | PF_CODE;
            if g.chance(85) {
                f |= PF_W;
            }
            if g.chance(15) {
                f |= PF_SLOW;
            }
            f
        } else if g.chance(15) {
            0
        } else {
            // Drawn to exercise the fast load mask, the fast store mask and the fall-through.
            let mut f = PF_R;
            if g.chance(80) {
                f |= PF_W;
            }
            if g.chance(15) {
                f |= PF_SLOW;
            }
            if g.chance(10) {
                f &= !PF_R;
            }
            f
        };
    }

    let mut x = [0u32; 32];
    for (i, r) in x.iter_mut().enumerate().skip(1) {
        *r = if i < 8 {
            BASE + g.below(PAGES * PAGE)
        } else {
            g.u32()
        };
    }

    // Dead and stop pages must not be reachable through the fast paths, or the page table would
    // promise what the bus does not deliver.
    let dead_page = g
        .chance(20)
        .then(|| CODE_PAGES + g.below(PAGES - CODE_PAGES));
    let stop_page = g
        .chance(30)
        .then(|| CODE_PAGES + g.below(PAGES - CODE_PAGES));
    if let Some(p) = dead_page {
        flags[p as usize] = 0;
    }
    if let Some(p) = stop_page {
        flags[p as usize] = PF_SLOW;
    }

    let spmon = if g.chance(25) {
        let min = BASE + g.below(PAGES * PAGE);
        SpMonitor {
            on_min: g.chance(70),
            on_max: g.chance(50),
            min,
            max: min.wrapping_add(g.below(0x1000)),
        }
    } else {
        SpMonitor::default()
    };

    Plan {
        image,
        flags,
        entry,
        stop_page,
        dead_page,
        spmon,
        x,
        mstatus: if g.chance(50) { 0x1888 } else { 0 },
        retire: 1 + (u64::from(g.u32()) % MAX_RETIRED),
        costs: InsnCosts::default(),
        chop: None,
    }
}

/// [`plan`] plus, for the costed leg, an optional `PF_MMIO` page with a local MMIO window, a
/// random class cost table and random run budgets.
fn costed_plan(g: &mut Gen) -> Plan {
    let mut p = plan(g);
    let mut window = (0, 0);
    if g.chance(50) {
        let page = CODE_PAGES + g.below(PAGES - CODE_PAGES);
        if p.dead_page != Some(page) {
            p.flags[page as usize] = PF_MMIO | (p.flags[page as usize] & PF_SLOW);
            if g.chance(50) {
                window = (BASE + page * PAGE + g.below(2) * (PAGE / 2), PAGE / 2);
            }
        }
    }
    // Code and data share the SRAM bank, so dense memory runs pay the bank penalty.
    let bank_len = if g.chance(50) { PAGES * PAGE } else { 0 };
    p.costs = InsnCosts {
        taken_branch: g.below(4),
        jump: g.below(3),
        split_redirect: g.below(3),
        load_use: g.below(3),
        div_base: g.below(20),
        mulh: g.below(5),
        mmio_load: g.below(8),
        mmio_store: g.below(9),
        mmio_local: g.below(4),
        local_mmio_base: window.0,
        local_mmio_len: window.1,
        bank: g.below(3),
        bank_code_base: BASE,
        bank_data_base: BASE,
        bank_len,
    };
    p.chop = Some(u64::from(g.u32()) | 1);
    p
}

/// The slow paths serve every image address whatever the page table says, so a fast path that
/// disagrees with them is an engine bug.
struct FuzzBus {
    arena: Vec<u8>,
    pages: PageTable,
    stop_page: Option<u32>,
    dead_page: Option<u32>,
    spmon: SpMonitor,
    custom: BTreeMap<u16, u32>,
    /// Code-page byte ranges written since the last drain, for `Engine::invalidate_vrange`.
    dirty: Vec<(u32, u32)>,
}

impl FuzzBus {
    fn new(plan: &Plan) -> FuzzBus {
        let mut pages = PageTable::new();
        for (p, flags) in plan.flags.iter().enumerate() {
            let entry = if *flags == 0 {
                0
            } else {
                (p as u32 * PAGE) | flags
            };
            pages.set_entry((BASE >> 12) + p as u32, entry);
        }
        FuzzBus {
            arena: plan.image.clone(),
            pages,
            stop_page: plan.stop_page,
            dead_page: plan.dead_page,
            spmon: plan.spmon,
            custom: BTreeMap::new(),
            dirty: Vec::new(),
        }
    }

    fn take_dirty(&mut self) -> Vec<(u32, u32)> {
        std::mem::take(&mut self.dirty)
    }

    fn offset(&self, addr: u32, size: u8) -> Option<usize> {
        let off = addr.checked_sub(BASE)? as usize;
        if off + usize::from(size) > self.arena.len() {
            return None;
        }
        if self.dead_page == Some(off as u32 / PAGE) {
            return None;
        }
        Some(off)
    }
}

impl Bus for FuzzBus {
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
        let page = off as u32 / PAGE;
        if page < CODE_PAGES {
            // A store into code invalidates that page's blocks and answers `OkStop`.
            self.dirty.push((addr, u32::from(size)));
            return Access::OkStop(());
        }
        if self.stop_page == Some(page) {
            Access::OkStop(())
        } else {
            Access::Ok(())
        }
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
        let end = ((off / PAGE as usize) + 1) * PAGE as usize;
        Ok(CodePage {
            bytes: &self.arena[off..end.min(self.arena.len())],
        })
    }

    fn csr_custom(&mut self, csr: u16, op: CsrOp, _insns: u64) -> Result<(u32, CsrEffect), Trap> {
        let slot = self.custom.entry(csr).or_default();
        let before = *slot;
        if let CsrOp::Write(v) = op {
            *slot = v;
        }
        Ok((before, CsrEffect::None))
    }

    fn wfi_wake(&mut self) -> bool {
        // The driver clears `Hart::wfi` itself, identically for both sides.
        false
    }

    fn pmp_changed(&mut self, _csr: &Csr) {}
}

fn hart(plan: &Plan) -> Hart {
    let mut csr = Csr::new();
    csr.mtvec = VECTOR | 1;
    csr.mstatus = plan.mstatus;
    Hart {
        x: plan.x,
        pc: BASE + plan.entry,
        csr,
        wfi: false,
        insns: 0,
        stores: 0,
        spmon: plan.spmon,
        extra: 0,
        pipe: Default::default(),
    }
}

#[derive(PartialEq, Eq)]
struct State {
    x: [u32; 32],
    pc: u32,
    insns: u64,
    stores: u64,
    wfi: bool,
    spmon: (bool, bool, u32, u32),
    /// `extra` and `pipe` (bank state above the low byte) stay 0 without a cost table.
    extra: u64,
    pipe: u64,
    csr: Vec<u32>,
    custom: Vec<(u16, u32)>,
    memory: Vec<u8>,
}

fn state(hart: &Hart, bus: &FuzzBus) -> State {
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
        extra: hart.extra,
        pipe: u64::from(hart.pipe.to_byte()) | (hart.pipe.bank.to_bits() << 8),
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
    if a.wfi != b.wfi {
        return format!("wfi: reference {}, engine {}", a.wfi, b.wfi);
    }
    if a.spmon != b.spmon {
        return format!("spmon: reference {:?}, engine {:?}", a.spmon, b.spmon);
    }
    if a.extra != b.extra {
        return format!("extra cycles: reference {}, engine {}", a.extra, b.extra);
    }
    if a.pipe != b.pipe {
        return format!("pipe: reference {:#x}, engine {:#x}", a.pipe, b.pipe);
    }
    for (i, (x, y)) in a.csr.iter().zip(b.csr.iter()).enumerate() {
        if x != y {
            return format!("csr digest slot {i}: reference 0x{x:08x}, engine 0x{y:08x}");
        }
    }
    if a.custom != b.custom {
        return format!(
            "unknown CSRs: reference {:x?}, engine {:x?}",
            a.custom, b.custom
        );
    }
    for (i, (x, y)) in a.memory.iter().zip(b.memory.iter()).enumerate() {
        if x != y {
            return format!(
                "memory at 0x{:08x}: reference 0x{x:02x}, engine 0x{y:02x}",
                BASE + i as u32
            );
        }
    }
    "states differ in a field the reporter does not cover".to_string()
}

/// Returns the reference state and the count actually retired. Two passes, because a program that
/// cannot reach `Plan::retire` within [`MAX_STEPS`] must stop at a count the engine can match.
fn run_reference(plan: &Plan) -> (Hart, FuzzBus, u64) {
    let reachable = drive_reference(plan, plan.retire).0.insns;
    let (hart, bus) = drive_reference(plan, reachable);
    (hart, bus, reachable)
}

fn drive_reference(plan: &Plan, retire: u64) -> (Hart, FuzzBus) {
    let mut hart = hart(plan);
    let mut bus = FuzzBus::new(plan);
    let mut steps = 0;
    while hart.insns < retire && steps < MAX_STEPS {
        if let StepResult::Wfi = ref_step_costed(&mut hart, &mut bus, &plan.costs) {
            hart.wfi = false;
        }
        steps += 1;
    }
    (hart, bus)
}

fn run_engine(plan: &Plan, max_block_insns: u16, retire: u64) -> (Hart, FuzzBus) {
    let mut hart = hart(plan);
    let mut bus = FuzzBus::new(plan);
    let mut engine = Engine::new(EngineCfg {
        max_block_insns,
        ..EngineCfg::default()
    });
    engine.set_costs(Some(plan.costs));
    let hooks = HookSet::default();
    let mut chop = plan.chop.map(Gen::new);
    let mut calls = 0;
    while hart.insns < retire && calls < MAX_RUN_CALLS {
        // A costed budget is in clock units, retiring at most that many instructions, so the loop
        // still stops on `retire`; chopping checks that a mid-block stop never moves the clock.
        let left = retire - hart.insns;
        let budget = match chop.as_mut() {
            Some(g) => left.min(1 + u64::from(g.below(24))),
            None => left,
        };
        let exit = engine.run(&mut hart, &mut bus, &hooks, budget);
        // The store answered `OkStop`, so the engine already left the block: no stale op runs.
        for (addr, len) in bus.take_dirty() {
            engine.invalidate_vrange(addr, len);
        }
        match exit {
            Exit::Wfi => hart.wfi = false,
            // The stack-spill interrupt belongs to a peripheral this bus does not model.
            Exit::Stop | Exit::SpSpill(_) | Exit::Budget => {}
            Exit::Hook { .. } => unreachable!("the fuzz binds no hook"),
            // The engine's trap-loop break-out, where `ref_step` would spin: a candidate
            // divergence, so stop short and let the `insns` comparison report it with the seed.
            Exit::Halted(_) => break,
        }
        calls += 1;
    }
    (hart, bus)
}

fn sizing() -> (u32, u64) {
    let iterations = std::env::var("PEMU_FUZZ_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_ITERATIONS);
    let seed = std::env::var("PEMU_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    (iterations, seed)
}

/// `cargo xtask ci t0` runs this as its `fuzz-1e4` step.
#[test]
fn the_engine_equals_ref_step_at_block_sizes_1_3_and_64() {
    let (iterations, base_seed) = sizing();
    fuzz(iterations, base_seed);
}

/// The clock position must depend on the instruction stream alone: no block size, run slicing or
/// executor may move it.
#[test]
fn the_costed_engine_equals_ref_step_costed_at_block_sizes_1_3_and_64() {
    let (iterations, base_seed) = sizing();
    let mut charged = 0u64;
    let mut retired_total = 0u64;
    for iteration in 0..iterations {
        let seed = base_seed
            .wrapping_add(0xC057)
            .wrapping_add(u64::from(iteration).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let _guard = SeedOnPanic { iteration, seed };
        let plan = costed_plan(&mut Gen::new(seed));
        let (ref_hart, ref_bus, retired) = run_reference(&plan);
        charged += ref_hart.extra;
        retired_total += retired;
        let expected = state(&ref_hart, &ref_bus);
        for max_block_insns in BLOCK_SIZES {
            let (hart, bus) = run_engine(&plan, max_block_insns, retired);
            let got = state(&hart, &bus);
            assert!(
                expected == got,
                "costed iteration {iteration} (seed 0x{seed:016x}), max_block_insns \
                 {max_block_insns}, {retired} instructions, costs {:?}: {}",
                plan.costs,
                difference(&expected, &got)
            );
        }
    }
    println!(
        "cost fuzz: {iterations} costed programs, {retired_total} instructions and {charged} extra \
         cycles per block size"
    );
    assert!(
        charged > retired_total / 4,
        "the costed leg charged {charged} extra cycles over {retired_total} instructions: the \
         generator stopped reaching the class rows"
    );
}

/// The fixed 10^6 run. A debug build skips it: unoptimized it would take most of an hour.
#[test]
// The wall time is informational only; test code may use the clock the core crates ban.
#[allow(clippy::disallowed_types)]
fn t2_m0_ten_to_the_six_programs_equal_ref_step_at_block_sizes_1_3_and_64() {
    const TEST: &str = "t2_m0_ten_to_the_six_programs_equal_ref_step_at_block_sizes_1_3_and_64";
    if cfg!(debug_assertions) {
        println!(
            "SKIP {TEST}: 10^6 programs need the optimized build of the T2 tests (cargo test \
             --profile ci-test); T0 runs 10^4 in its `fuzz-1e4` step"
        );
        return;
    }
    let (_, base_seed) = sizing();
    let started = std::time::Instant::now();
    let (retired, traps) = fuzz(T2_ITERATIONS, base_seed);
    println!(
        "RAN {TEST} fuzz: {T2_ITERATIONS} programs from base seed 0x{base_seed:016x}, {retired} \
         instructions per block size, {traps} trapped, wall {:.1} s",
        started.elapsed().as_secs_f64()
    );
}

const T2_ITERATIONS: u32 = 1_000_000;

/// Prints the seed on any panic, including one inside the engine or bus.
struct SeedOnPanic {
    iteration: u32,
    seed: u64,
}

impl Drop for SeedOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "engine fuzz failed at iteration {} (seed 0x{:016x}); replay it with \
                 SEED=0x{:016x} cargo test -p pemu-rv32 --test fuzz -- --ignored replay_one_seed",
                self.iteration, self.seed, self.seed
            );
        }
    }
}

/// Returns the instructions retired per block size and how many programs trapped.
fn fuzz(iterations: u32, base_seed: u64) -> (u64, u32) {
    let mut retired_total = 0u64;
    let mut traps = 0u32;
    for iteration in 0..iterations {
        let seed = base_seed.wrapping_add(u64::from(iteration).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let _guard = SeedOnPanic { iteration, seed };
        let plan = plan(&mut Gen::new(seed));
        let (ref_hart, ref_bus, retired) = run_reference(&plan);
        if ref_hart.csr.mcause != 0 || ref_hart.csr.mepc != 0 {
            traps += 1;
        }
        retired_total += retired;
        let expected = state(&ref_hart, &ref_bus);
        for max_block_insns in BLOCK_SIZES {
            let (hart, bus) = run_engine(&plan, max_block_insns, retired);
            let got = state(&hart, &bus);
            assert!(
                expected == got,
                "iteration {iteration} (seed 0x{seed:016x}), max_block_insns {max_block_insns}, \
                 {retired} instructions: {}",
                difference(&expected, &got)
            );
        }
    }
    println!(
        "engine fuzz: {iterations} programs, {retired_total} instructions per block size, \
         {traps} programs took at least one trap"
    );
    assert!(
        traps * 10 >= iterations,
        "only {traps} of {iterations} programs trapped: the generator stopped covering traps"
    );
    (retired_total, traps)
}

#[test]
fn the_generator_is_reproducible_from_its_seed() {
    let a = plan(&mut Gen::new(0x1234_5678_9ABC_DEF0));
    let b = plan(&mut Gen::new(0x1234_5678_9ABC_DEF0));
    let c = plan(&mut Gen::new(0x1234_5678_9ABC_DEF1));
    assert_eq!(a.image, b.image);
    assert_eq!(a.flags, b.flags);
    assert_eq!(a.x, b.x);
    assert_eq!(a.retire, b.retire);
    assert_ne!(
        (a.image.clone(), a.x, a.retire),
        (c.image.clone(), c.x, c.retire),
        "two seeds must not give the same program"
    );
}

#[test]
fn a_replay_of_one_plan_is_identical() {
    let plan = plan(&mut Gen::new(0xFEED_FACE_CAFE_BEEF));
    let (ref_hart, ref_bus, retired) = run_reference(&plan);
    let expected = state(&ref_hart, &ref_bus);
    for _ in 0..2 {
        let (h, b) = run_engine(&plan, 64, retired);
        assert!(
            expected == state(&h, &b),
            "{}",
            difference(&expected, &state(&h, &b))
        );
    }
}

/// Replays one seed and reports the first instruction count at which the engine and `ref_step`
/// disagree:
///
/// ```sh
/// SEED=0x3dd36ed3fca48402 cargo test -p pemu-rv32 --test fuzz -- --ignored --nocapture
/// ```
#[test]
#[ignore = "reproduction helper for one seed, selected with SEED"]
fn replay_one_seed() {
    let seed: u64 = std::env::var("SEED")
        .ok()
        .and_then(|v| u64::from_str_radix(v.trim_start_matches("0x"), 16).ok())
        .unwrap_or(DEFAULT_SEED);
    let plan = plan(&mut Gen::new(seed));
    let (_, _, retired) = run_reference(&plan);
    println!(
        "seed 0x{seed:016x}: flags {:x?}, stop page {:?}, dead page {:?}, {retired} instructions",
        plan.flags, plan.stop_page, plan.dead_page
    );
    for max_block_insns in BLOCK_SIZES {
        for n in 1..=retired {
            let mut at = plan.clone();
            at.retire = n;
            let (ref_hart, ref_bus, got) = run_reference(&at);
            if got != n {
                break;
            }
            let (hart, bus) = run_engine(&at, max_block_insns, n);
            let expected = state(&ref_hart, &ref_bus);
            let actual = state(&hart, &bus);
            if expected != actual {
                println!(
                    "max_block_insns {max_block_insns}: first divergence after {n} instructions: {}",
                    difference(&expected, &actual)
                );
                println!(
                    "  reference pc 0x{:08x} mepc 0x{:08x} mcause {} mtval 0x{:08x}",
                    ref_hart.pc, ref_hart.csr.mepc, ref_hart.csr.mcause, ref_hart.csr.mtval
                );
                println!(
                    "  engine    pc 0x{:08x} mepc 0x{:08x} mcause {} mtval 0x{:08x}",
                    hart.pc, hart.csr.mepc, hart.csr.mcause, hart.csr.mtval
                );
                break;
            }
        }
    }
}
