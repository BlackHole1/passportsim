//! Machine-level tests of poll fast-forward, the hang detector and ROM delay fast-forward over the
//! bundled ROM and hand-assembled guests in SRAM1. Every fast-forward test runs a program with the
//! shortcut on and off and compares architectural state, instruction count, virtual time, trace
//! digest and console.

use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_rv32::bus::{Bus, HartView};

use crate::config::{Assets, MachineConfig, TraceCfg};
use crate::executor::Executor;
use crate::hang::{HangCfg, StuckKind};
use crate::machine::Machine;
use crate::run::{RunLimits, RunOutcome};
use crate::stops::{StopReason, StopSet};

const PROG: u32 = pemu_soc_c3::mem::SRAM1_IRAM_BASE;

mod asm {
    pub const A0: u32 = 10;
    pub const A1: u32 = 11;
    pub const A2: u32 = 12;
    pub const A4: u32 = 14;
    pub const RA: u32 = 1;
    pub const T0: u32 = 5;
    pub const T1: u32 = 6;

    pub fn lui(rd: u32, imm20: u32) -> u32 {
        (imm20 & 0xF_FFFF) << 12 | rd << 7 | 0x37
    }
    pub fn addi(rd: u32, rs1: u32, imm: i32) -> u32 {
        ((imm as u32) & 0xFFF) << 20 | rs1 << 15 | rd << 7 | 0x13
    }
    pub fn lw(rd: u32, rs1: u32, off: i32) -> u32 {
        ((off as u32) & 0xFFF) << 20 | rs1 << 15 | 2 << 12 | rd << 7 | 0x03
    }
    pub fn sw(rs2: u32, rs1: u32, off: i32) -> u32 {
        let o = off as u32;
        (o >> 5 & 0x7F) << 25 | rs2 << 20 | rs1 << 15 | 2 << 12 | (o & 0x1F) << 7 | 0x23
    }
    fn branch(funct3: u32, rs1: u32, rs2: u32, off: i32) -> u32 {
        let o = off as u32;
        (o >> 12 & 1) << 31
            | (o >> 5 & 0x3F) << 25
            | rs2 << 20
            | rs1 << 15
            | funct3 << 12
            | (o >> 1 & 0xF) << 8
            | (o >> 11 & 1) << 7
            | 0x63
    }
    pub fn beq(rs1: u32, rs2: u32, off: i32) -> u32 {
        branch(0, rs1, rs2, off)
    }
    pub fn bne(rs1: u32, rs2: u32, off: i32) -> u32 {
        branch(1, rs1, rs2, off)
    }
    pub fn jal(rd: u32, off: i32) -> u32 {
        let o = off as u32;
        (o >> 20 & 1) << 31
            | (o >> 1 & 0x3FF) << 21
            | (o >> 11 & 1) << 20
            | (o >> 12 & 0xFF) << 12
            | rd << 7
            | 0x6F
    }
    pub fn jalr(rd: u32, rs1: u32, off: i32) -> u32 {
        ((off as u32) & 0xFFF) << 20 | rs1 << 15 | rd << 7 | 0x67
    }
    pub fn csrwi(csr: u32, uimm: u32) -> u32 {
        csr << 20 | uimm << 15 | 5 << 12 | 0x73
    }
    pub fn csrr(rd: u32, csr: u32) -> u32 {
        csr << 20 | 2 << 12 | rd << 7 | 0x73
    }
    pub fn li(rd: u32, val: u32) -> [u32; 2] {
        let lo = ((val & 0xFFF) as i32) << 20 >> 20;
        let hi = val.wrapping_sub(lo as u32) >> 12;
        [lui(rd, hi), addi(rd, rd, lo)]
    }
}

fn machine(poll_ff: bool) -> Machine {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let cfg = MachineConfig {
        poll_ff,
        trace: TraceCfg::all(),
        ..MachineConfig::default()
    };
    let mut m = Machine::new(cfg, assets).expect("the ROM fits the ROM window");
    // The flash-boot RWDT hold would reset the chip 2.94 s in; clear it as the bootloader does.
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| {
        bus.store_slow(
            0x6000_80A8,
            4,
            pemu_soc_c3::periph::rtc_cntl::WDT_WKEY,
            &view,
        );
        bus.store_slow(0x6000_8090, 4, 0, &view);
        bus.store_slow(0x6000_80A8, 4, 0, &view);
    });
    m
}

fn load(m: &mut Machine, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        let stored = m.soc.store_mem(PROG + 4 * i as u32, 4, *w);
        assert!(matches!(stored, pemu_soc_c3::Stored::Wrote { .. }));
    }
    m.hart.pc = PROG;
}

fn until_ms(ms: u64) -> RunLimits {
    RunLimits {
        until: Some(VTime::from_ms(ms)),
        max_insns: None,
        stops: StopSet::default(),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Observed {
    x: [u32; 32],
    pc: u32,
    insns: u64,
    stores: u64,
    now: VTime,
    mstatus: u32,
    trace: [u8; 32],
    last_read: Option<crate::machine::MmioRead>,
    sram: Vec<u8>,
}

fn observe(m: &Machine) -> Observed {
    Observed {
        x: m.hart.x,
        pc: m.hart.pc,
        insns: m.hart.insns,
        stores: m.hart.stores,
        now: m.now(),
        mstatus: m.hart.csr.mstatus,
        trace: m.trace.digest(),
        last_read: m.last_mmio_read(),
        sram: (0..0x400u32)
            .map(|i| m.soc.load_mem(0x3FC8_8000 + i, 1).unwrap_or(0) as u8)
            .collect(),
    }
}

fn on_and_off(
    prog: &[u32],
    setup: impl Fn(&mut Machine),
    lim: impl Fn() -> RunLimits,
) -> [(RunOutcome, Observed); 2] {
    [true, false].map(|ff| {
        let mut m = machine(ff);
        m.set_rom_delay_ff(ff);
        setup(&mut m);
        load(&mut m, prog);
        let out = m.run(lim());
        let seen = observe(&m);
        (out, seen)
    })
}

/// `lui a0, 0x60000; loop: lw a1, 0x1C(a0); j loop`: UART0 `UART_STATUS`, which holds until an
/// input, polled for ever with no store.
fn uart_poll() -> Vec<u32> {
    use asm::*;
    vec![lui(A0, 0x60000), lw(A1, A0, 0x1C), jal(0, -4)]
}

#[test]
fn a_confirmed_poll_is_fast_forwarded_with_identical_state_and_trace() {
    // The USJ SOF tick ends the chain every millisecond, so the loop confirms and skips again
    // up to each tick.
    let [(on, seen_on), (off, seen_off)] = on_and_off(&uart_poll(), |_| {}, || until_ms(20));
    assert_eq!(on.reason, StopReason::Until);
    assert_eq!(off.reason, StopReason::Until);
    assert_eq!(
        seen_on, seen_off,
        "poll fast-forward changed what the guest sees"
    );
    assert_eq!(on.insns, off.insns);
    assert_eq!(off.ff_insns, 0);
    assert!(
        on.ff_insns > on.insns / 2,
        "most of the loop is skipped: {} of {}",
        on.ff_insns,
        on.insns
    );
    // The trace folds the read into poll runs and the tap counts every iteration, skipped or
    // not.
    assert!(seen_on.last_read.expect("reads happened").repeats > 1000);
}

#[test]
fn a_poll_fast_forward_stops_on_the_exact_instruction_limit() {
    let lim = || RunLimits::insns(1_000_003);
    let [(on, seen_on), (off, seen_off)] = on_and_off(&uart_poll(), |_| {}, lim);
    assert_eq!(on.reason, StopReason::MaxInsns);
    assert_eq!(off.reason, StopReason::MaxInsns);
    assert_eq!(on.insns, 1_000_003);
    assert_eq!(seen_on, seen_off);
    assert!(on.ff_insns > 0);
}

#[test]
fn a_run_split_into_pieces_ends_where_one_run_ends() {
    let mut whole = machine(true);
    load(&mut whole, &uart_poll());
    whole.run(until_ms(12));
    let mut pieces = machine(true);
    load(&mut pieces, &uart_poll());
    for ms in [1, 2, 5, 7, 11, 12] {
        pieces.run(until_ms(ms));
    }
    pieces.run(RunLimits::insns(0));
    assert_eq!(observe(&whole).x, observe(&pieces).x);
    assert_eq!(whole.hart.insns, pieces.hart.insns);
    assert_eq!(whole.now(), pieces.now());
    assert_eq!(whole.trace.digest(), pieces.trace.digest());
}

#[test]
fn the_reference_executor_and_the_engine_agree_with_fast_forward_on() {
    let mut engine = machine(true);
    load(&mut engine, &uart_poll());
    engine.run(until_ms(5));
    let mut reference = machine(true);
    reference.set_executor(Executor::Reference);
    load(&mut reference, &uart_poll());
    reference.run(until_ms(5));
    assert_eq!(observe(&engine), observe(&reference));
}

#[test]
fn tracing_on_or_off_leaves_the_run_unchanged() {
    let run = |trace: TraceCfg| {
        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("pinned");
        let cfg = MachineConfig {
            poll_ff: true,
            trace,
            ..MachineConfig::default()
        };
        let mut m = Machine::new(cfg, assets).expect("builds");
        load(&mut m, &uart_poll());
        let out = m.run(until_ms(8));
        (out.insns, out.ff_insns, m.hart.x, m.now())
    };
    assert_eq!(run(TraceCfg::all()), run(TraceCfg::default()));
}

/// `SPI_CMD` of SPI2 with `SPI_UPDATE` (bit 23) set, then polled until the bit clears: IDF's
/// `spi_ll_apply_config` (the `spi2.cmd_update` row), then `j .`.
fn spi2_update_wait() -> Vec<u32> {
    use asm::*;
    vec![
        lui(A0, 0x60024),
        lui(A1, 0x800),
        sw(A1, A0, 0),
        lw(A2, A0, 0),
        bne(A2, 0, -4),
        jal(0, 0),
    ]
}

#[test]
fn a_poll_on_a_disabled_model_is_stuck_naming_its_busy_wait_row() {
    // With spi2 disabled nothing can clear SPI_UPDATE, so the `Unchangeable` row fires at the
    // confirmation.
    for ff in [true, false] {
        let mut m = machine(ff);
        assert!(m.disable_model("spi2"));
        load(&mut m, &spi2_update_wait());
        let out = m.run(until_ms(3_000));
        let StopReason::Stuck(report) = out.reason else {
            panic!("expected Stuck, got {:?}", out.reason);
        };
        assert_eq!(report.kind, StuckKind::Unchangeable);
        assert_eq!(report.block, "spi2");
        assert_eq!(report.register, "SPI_CMD");
        assert_eq!(report.wait_row, Some("spi2.cmd_update"));
        assert_eq!(report.expect, Some("read SPI_UPDATE == 0"));
        assert_eq!(report.pc, PROG + 12);
        assert_eq!(report.val, 1 << 23);
        assert!(out.vt < VTime::from_ms(1), "{:?}", out.vt);
    }
}

#[test]
fn with_the_model_enabled_the_same_wait_completes() {
    let mut m = machine(true);
    load(&mut m, &spi2_update_wait());
    let out = m.run(until_ms(50));
    assert_eq!(out.reason, StopReason::Until);
    assert_eq!(m.hart.pc, PROG + 20, "the model cleared SPI_UPDATE");
}

/// `SHA_BUSY` polled while it reads 0, for ever: a modeled register whose value never changes.
/// With `store`, each iteration also stores into SRAM.
fn sha_busy_poll(store: bool) -> Vec<u32> {
    use asm::*;
    if store {
        vec![
            lui(A0, 0x6003B),
            lui(T1, 0x3FC88),
            lw(A1, A0, 0x18),
            sw(A1, T1, 0),
            beq(A1, 0, -8),
        ]
    } else {
        vec![lui(A0, 0x6003B), lw(A1, A0, 0x18), beq(A1, 0, -4)]
    }
}

fn short_hang(m: &mut Machine) {
    m.set_hang_detector(HangCfg {
        enabled: true,
        stuck_ms: 50,
    });
}

#[test]
fn an_unchanged_modeled_register_is_stuck_after_stuck_ms_at_the_same_instruction() {
    let [(on, seen_on), (off, seen_off)] =
        on_and_off(&sha_busy_poll(false), short_hang, || until_ms(1_000));
    for out in [&on, &off] {
        let StopReason::Stuck(report) = &out.reason else {
            panic!("expected Stuck, got {:?}", out.reason);
        };
        assert_eq!(report.kind, StuckKind::Unchanged);
        assert_eq!(report.block, "sha");
        assert_eq!(report.wait_row, Some("sha.busy"));
        assert_eq!(report.pc, PROG + 4);
        assert!(report.at.0 - report.since.0 >= VTime::from_ms(50).0);
        assert!(report.at < VTime::from_ms(52), "{:?}", report.at);
    }
    assert_eq!(on.reason, off.reason);
    assert_eq!(seen_on, seen_off);
    assert!(on.ff_insns > 0);
}

#[test]
fn a_storing_loop_is_caught_by_the_fallback_row() {
    let [(on, seen_on), (off, seen_off)] =
        on_and_off(&sha_busy_poll(true), short_hang, || until_ms(1_000));
    let StopReason::Stuck(report) = &on.reason else {
        panic!("expected Stuck, got {:?}", on.reason);
    };
    assert_eq!(report.kind, StuckKind::Fallback);
    assert_eq!(report.wait_row, Some("sha.busy"));
    assert_eq!(on.reason, off.reason);
    assert_eq!(seen_on, seen_off);
    assert_eq!(on.ff_insns, 0, "a loop that stores is never fast-forwarded");
}

#[test]
fn a_register_that_changes_only_through_input_is_never_a_hang() {
    let mut m = machine(true);
    short_hang(&mut m);
    load(&mut m, &uart_poll());
    let out = m.run(until_ms(300));
    assert_eq!(out.reason, StopReason::Until);
}

#[test]
fn a_disabled_detector_lets_the_hang_run_to_its_limit() {
    let mut m = machine(true);
    m.set_hang_detector(HangCfg {
        enabled: false,
        ..HangCfg::default()
    });
    assert!(m.disable_model("spi2"));
    load(&mut m, &spi2_update_wait());
    assert_eq!(m.run(until_ms(10)).reason, StopReason::Until);
}

/// Enables the cycle counter, reads it into `a4`, calls the rev101 `ets_delay_us` loop head
/// 0x40047e9e with `a0 = cycles`, then spins at `PROG + 36`.
fn rom_delay(cycles: u32) -> Vec<u32> {
    use asm::*;
    let [a0_hi, a0_lo] = li(A0, cycles);
    vec![
        csrwi(0x7E0, 1),
        csrwi(0x7E1, 1),
        csrr(A4, 0x802),
        a0_hi,
        a0_lo,
        lui(T0, 0x40048),
        jalr(RA, T0, -0x162),
        addi(0, 0, 0),
        addi(0, 0, 0),
        jal(0, 0),
    ]
}

const AFTER_DELAY: u32 = PROG + 28;

#[test]
fn the_bundled_rev101_rom_has_its_delay_loop_pinned() {
    let m = machine(true);
    assert_eq!(m.rom_delay.head, Some(0x4004_7E9E));
    assert!(m.rom_delay_ff());
}

#[test]
fn a_rom_delay_is_fast_forwarded_with_identical_state() {
    // 400,000 cycles at 40 MHz: 10 ms, crossing ten SOF ticks.
    let [(on, seen_on), (off, seen_off)] = on_and_off(&rom_delay(400_000), |_| {}, || until_ms(30));
    assert_eq!(seen_on, seen_off);
    assert_eq!(on.insns, off.insns);
    assert_eq!(seen_on.pc, PROG + 36, "the delay returned");
    assert!(on.ff_insns > 300_000, "{} skipped", on.ff_insns);
    assert_eq!(off.ff_insns, 0);
}

#[test]
fn a_rom_delay_ends_on_the_same_instruction_with_the_shortcut_on_or_off() {
    let lim = || RunLimits {
        until: None,
        max_insns: Some(10_000_000),
        stops: StopSet {
            breakpoints: vec![AFTER_DELAY],
            ..StopSet::default()
        },
    };
    let [(on, seen_on), (off, seen_off)] = on_and_off(&rom_delay(400_000), |_| {}, lim);
    assert_eq!(on.reason, StopReason::Breakpoint(AFTER_DELAY));
    assert_eq!(off.reason, on.reason);
    assert_eq!(seen_on, seen_off);
    let mid = || RunLimits::insns(123_457);
    let [(on, seen_on), (_, seen_off)] = on_and_off(&rom_delay(400_000), |_| {}, mid);
    assert_eq!(on.insns, 123_457);
    assert_eq!(seen_on, seen_off);
    assert!(on.ff_insns > 0);
}

#[test]
fn a_breakpoint_inside_the_delay_loop_sees_every_iteration() {
    let mut m = machine(true);
    load(&mut m, &rom_delay(400));
    let lim = || RunLimits {
        until: None,
        max_insns: Some(1_000_000),
        stops: StopSet {
            breakpoints: vec![0x4004_7E9E],
            ..StopSet::default()
        },
    };
    let mut hits = 0;
    while m.run(lim()).reason == StopReason::Breakpoint(0x4004_7E9E) {
        hits += 1;
        assert!(hits < 1_000);
    }
    assert!(hits > 100, "{hits} iterations seen");
    assert_eq!(m.ff_insns, 0);
}

#[test]
fn the_reference_executor_takes_the_rom_delay_shortcut_too() {
    let mut m = machine(true);
    m.set_executor(Executor::Reference);
    load(&mut m, &rom_delay(400_000));
    let out = m.run(until_ms(30));
    let mut e = machine(true);
    load(&mut e, &rom_delay(400_000));
    e.run(until_ms(30));
    assert!(out.ff_insns > 0);
    assert_eq!(observe(&m), observe(&e));
}

// Regression tests around breakpoints, run ends and single reads near confirmed poll loops.

#[test]
fn review_a_reference_breakpoint_inside_poll_loop() {
    let run = |ff: bool| {
        let mut m = machine(ff);
        m.set_executor(Executor::Reference);
        load(&mut m, &uart_poll());
        let mut stops = Vec::new();
        for _ in 0..200 {
            let out = m.run(RunLimits {
                until: Some(VTime::from_ms(50)),
                max_insns: None,
                stops: StopSet {
                    breakpoints: vec![PROG + 8],
                    ..StopSet::default()
                },
            });
            stops.push((out.reason.clone(), m.hart.insns));
        }
        stops
    };
    let on = run(true);
    let off = run(false);
    let first = on.iter().zip(off.iter()).position(|(a, b)| a != b);
    println!(
        "A: first divergence at call {:?}: on {:?} off {:?}",
        first,
        first.map(|i| &on[i]),
        first.map(|i| &off[i])
    );
    assert_eq!(on, off);
}

#[test]
fn review_a2_engine_breakpoint_inside_poll_loop() {
    let run = |ff: bool| {
        let mut m = machine(ff);
        load(&mut m, &uart_poll());
        let mut stops = Vec::new();
        for _ in 0..200 {
            let out = m.run(RunLimits {
                until: Some(VTime::from_ms(50)),
                max_insns: None,
                stops: StopSet {
                    breakpoints: vec![PROG + 8],
                    ..StopSet::default()
                },
            });
            stops.push((out.reason.clone(), m.hart.insns));
        }
        stops
    };
    assert_eq!(run(true), run(false));
}

/// A run ends between a fast-forward bounded by the hang deadline and the next read; the next
/// run arms a breakpoint right after the read.
#[test]
fn review_d_engine_breakpoint_armed_after_again() {
    let mut diverged = Vec::new();
    for n in 0..40u64 {
        let run = |ff: bool| {
            let mut m = machine(ff);
            m.set_hang_detector(HangCfg {
                enabled: true,
                stuck_ms: 50,
            });
            let now = m.now();
            m.soc.devices.usj.set_link(
                pemu_soc_c3::periph::usj::HostLink::DETACHED,
                now,
                &mut m.sched,
            );
            load(&mut m, &sha_busy_poll(false));
            let a = m.run(RunLimits {
                until: Some(VTime::from_ms(1000)),
                max_insns: Some(1_000_000 + n),
                stops: StopSet::default(),
            });
            let b = m.run(RunLimits {
                until: Some(VTime::from_ms(1000)),
                max_insns: None,
                stops: StopSet {
                    breakpoints: vec![PROG + 8],
                    ..StopSet::default()
                },
            });
            (a.reason, a.insns, b.reason, b.insns, m.hart.insns)
        };
        let (on, off) = (run(true), run(false));
        if on != off {
            diverged.push((n, on, off));
        }
    }
    println!("D: {:?}", diverged.iter().take(3).collect::<Vec<_>>());
    assert!(diverged.is_empty());
}

/// A register read once, 75 ms of storing work without idle, read once more at the same pc,
/// then a spin with no reads: not a busy-wait.
#[test]
fn review_b_fallback_two_reads_far_apart() {
    use asm::*;
    let prog = vec![
        lui(A0, 0x6003B), // 0
        lui(T1, 0x3FC88), // 4
        lw(A1, A0, 0x18), // 8   read
        lui(T0, 0x100),   // 12  t0 = 0x100000
        sw(T0, T1, 0),    // 16
        addi(T0, T0, -1), // 20
        bne(T0, 0, -8),   // 24
        lw(A1, A0, 0x18), // 28  read again, 20 bytes away
        jal(0, 0),        // 32  j .
    ];
    let mut m = machine(true);
    short_hang(&mut m);
    load(&mut m, &prog);
    let out = m.run(until_ms(300));
    println!("B: {:?} at {:?}", out.reason, out.vt);
    assert_eq!(out.reason, StopReason::Until);
}

/// The `Unchanged` candidate survives leaving the loop; one later single read at that pc stops.
#[test]
fn review_c_unchanged_candidate_survives_leaving_loop() {
    use asm::*;
    let prog = vec![
        lui(A0, 0x6003B), // 0
        lw(A1, A0, 0x18), // 4  poll
        beq(A1, 0, -4),   // 8
        jal(0, 0),        // 12 (unused)
        // 16: other work, stores, no reads
        lui(T1, 0x3FC88), // 16
        lui(T0, 0x100),   // 20
        sw(T0, T1, 0),    // 24
        addi(T0, T0, -1), // 28
        bne(T0, 0, -8),   // 32
        lw(A1, A0, 0x18), // 36 unrelated single read? no: jump back to pc 4 once
        jal(0, 0),        // 40
    ];
    let mut m = machine(true);
    short_hang(&mut m);
    load(&mut m, &prog);
    let out = m.run(until_ms(10));
    println!("C1: {:?}", out.reason);
    // leave the loop, as an ISR changing a RAM flag would
    m.hart.pc = PROG + 16;
    let out = m.run(until_ms(100));
    println!("C2: {:?} pc {:#x}", out.reason, m.hart.pc);
    // now a single read at pc 4 (one call of the poll function whose flag is already set)
    m.hart.x[11] = 1; // irrelevant; the read overwrites a1
    m.hart.pc = PROG + 4;
    let out = m.run(RunLimits::insns(1));
    println!("C3: {:?}", out.reason);
    assert!(!matches!(out.reason, StopReason::Stuck(_)));
}

/// A loop whose period was measured across a clock stall is never confirmed on it.
#[test]
fn a_clock_stall_between_reads_ends_the_poll_chain() {
    let mut m = machine(true);
    let read = |m: &mut Machine, insns: u64| {
        let view = HartView {
            insns,
            extra: 0,
            pc: PROG,
        };
        m.with_bus(|bus, _| bus.load_slow(0x6000_001C, 4, &view));
        m.poll.want_check
    };
    let mut asked = 0;
    for i in 0..(crate::poll_ff::CONFIRM_REPEATS - 1) {
        asked += u64::from(read(&mut m, 10 + 2 * i));
    }
    assert_eq!(asked, 0, "63 repeats do not confirm yet");
    let mut control = machine(true);
    for i in 0..(crate::poll_ff::CONFIRM_REPEATS - 1) {
        read(&mut control, 10 + 2 * i);
    }
    assert!(read(&mut control, 10 + 2 * 63), "the 64th repeat asks");
    m.clock.stall(m.hart.insns, 1_000, false);
    assert!(
        !read(&mut m, 10 + 2 * 63),
        "the stall did not end the chain"
    );
}

/// The stop lands on the same instruction with the fast-forward on or off.
#[test]
fn a_long_wait_with_a_pending_completion_event_is_not_a_hang_until_the_event() {
    let setup = |m: &mut Machine| {
        short_hang(m);
        let now = m.now();
        // An SHA event 200 ms out whose tag the model ignores: a late completion that changes
        // nothing.
        m.sched.schedule(
            now,
            VTime::from_ms(200),
            pemu_core::sched::EventKey {
                owner: pemu_core::sched::Owner::Periph(pemu_soc_c3::periph::id::SHA),
                tag: 0x7FFF,
            },
        );
    };
    let [(on, seen_on), (off, seen_off)] =
        on_and_off(&sha_busy_poll(false), setup, || until_ms(1_000));
    let StopReason::Stuck(report) = &on.reason else {
        panic!("expected Stuck after the event, got {:?}", on.reason);
    };
    assert_eq!(report.kind, StuckKind::Unchanged);
    assert!(report.at >= VTime::from_ms(200), "{:?}", report.at);
    assert!(report.at < VTime::from_ms(201), "{:?}", report.at);
    assert_eq!(on.reason, off.reason);
    assert_eq!(seen_on, seen_off);
}

/// A hole of the peripheral window reads 0 and is not "unchangeable": the `Unchanged` row
/// reports it after `stuck_ms`.
#[test]
fn a_poll_of_an_unclaimed_address_is_judged_by_the_unchanged_row() {
    use asm::*;
    // 0x6000_1000 is a hole between UART0 and SPI1.
    let prog = vec![lui(A0, 0x60001), lw(A1, A0, 0), beq(A1, 0, -4)];
    let mut m = machine(true);
    short_hang(&mut m);
    load(&mut m, &prog);
    let out = m.run(until_ms(1_000));
    let StopReason::Stuck(report) = out.reason else {
        panic!("expected Stuck, got {:?}", out.reason);
    };
    assert_eq!(report.kind, StuckKind::Unchanged);
    assert_eq!(report.block, crate::hang::UNMAPPED_BLOCK);
    assert!(report.at >= VTime::from_ms(50));
}

#[test]
fn the_rom_delay_hook_returns_after_a_breakpoint_at_the_loop_head() {
    let mut m = machine(true);
    m.set_rom_delay_ff(false);
    load(&mut m, &rom_delay(400_000));
    let out = m.run(RunLimits {
        until: None,
        max_insns: Some(1_000_000),
        stops: StopSet {
            breakpoints: vec![0x4004_7E9E],
            ..StopSet::default()
        },
    });
    assert_eq!(out.reason, StopReason::Breakpoint(0x4004_7E9E));
    m.set_rom_delay_ff(true);
    let out = m.run(until_ms(30));
    assert_eq!(
        m.hooks.get(0x4004_7E9E),
        Some(crate::rom_delay::delay_hook_id()),
        "the shortcut's hook is not installed"
    );
    assert!(out.ff_insns > 300_000, "{} skipped", out.ff_insns);
}

#[test]
fn a_failed_confirmation_backs_off_exponentially() {
    use crate::poll_ff::{CONFIRM_REPEATS, MAX_BACKOFF, PollTracker};
    let mut t = PollTracker::default();
    assert_eq!(t.threshold(PROG, 0x6000_001C), CONFIRM_REPEATS);
    for n in 1..=3 {
        t.failed(PROG, 0x6000_001C);
        assert_eq!(t.threshold(PROG, 0x6000_001C), CONFIRM_REPEATS << n);
    }
    assert_eq!(t.threshold(PROG + 4, 0x6000_001C), CONFIRM_REPEATS);
    for _ in 0..20 {
        t.failed(PROG, 0x6000_001C);
    }
    assert_eq!(
        t.threshold(PROG, 0x6000_001C),
        CONFIRM_REPEATS << MAX_BACKOFF
    );
    t.confirmed(PROG, 0x6000_001C);
    assert_eq!(t.threshold(PROG, 0x6000_001C), CONFIRM_REPEATS);
}

/// A loop whose state never repeats backs off until a confirmation no longer fits between two
/// SOF ticks. Out of reset the CPU runs at 20 MHz, where a 3-instruction iteration reads about
/// 6,667 times per millisecond, so the threshold stops at 64 << 7.
#[test]
fn a_counting_loop_backs_off_until_it_is_no_longer_checked() {
    use asm::*;
    // lui a0; loop: lw a1, 0x1C(a0); addi a2, a2, 1; j loop.
    let prog = vec![
        lui(A0, 0x60000),
        lw(A1, A0, 0x1C),
        addi(A2, A2, 1),
        jal(0, -8),
    ];
    let mut m = machine(true);
    load(&mut m, &prog);
    m.run(until_ms(50));
    assert_eq!(
        m.poll.threshold(PROG + 4, 0x6000_001C),
        crate::poll_ff::CONFIRM_REPEATS << 7
    );
    assert_eq!(m.ff_insns, 0);
}

#[test]
fn a_disabled_model_receives_none_of_its_scheduled_events() {
    let mut m = machine(true);
    let now = m.now();
    m.sched.schedule(
        now,
        VTime::from_ms(1),
        pemu_core::sched::EventKey {
            owner: pemu_core::sched::Owner::Periph(pemu_soc_c3::periph::id::SPI2),
            tag: 0,
        },
    );
    assert!(m.disable_model("spi2"));
    load(&mut m, &uart_poll());
    m.run(until_ms(2));
    assert_eq!(m.disabled_model_events(), 1);
    assert_eq!(m.undispatched_events(), 0);
}

/// The instant, pc and instruction count at which `m` reports `Stuck`, running in `step_ms`
/// pieces and calling `between` at every pause.
fn stuck_in_pieces(
    m: &mut Machine,
    step_ms: u64,
    mut between: impl FnMut(&mut Machine),
) -> (StuckKind, VTime, VTime, u64) {
    for i in 1..=200 {
        let out = m.run(until_ms(step_ms * i));
        match out.reason {
            StopReason::Until => between(m),
            StopReason::Stuck(r) => return (r.kind, r.since, r.at, m.hart.insns),
            other => panic!("expected Stuck or Until, got {other:?}"),
        }
    }
    panic!("no hang in {} ms", step_ms * 200);
}

#[test]
fn a_hang_is_reported_at_the_same_instant_across_pauses_and_hang_section_restores() {
    for (prog, kind) in [
        (sha_busy_poll(false), StuckKind::Unchanged),
        (sha_busy_poll(true), StuckKind::Fallback),
    ] {
        let run = |step_ms: u64, restore: bool| {
            let mut m = machine(true);
            short_hang(&mut m);
            load(&mut m, &prog);
            stuck_in_pieces(&mut m, step_ms, |m| {
                if restore {
                    let section = m.hang_section();
                    let bytes = pemu_core::snap::SnapSection::encode(&section).unwrap();
                    let back: crate::poll_ff::HangSection =
                        pemu_core::snap::SnapSection::decode(&bytes).unwrap();
                    assert_eq!(back, section);
                    m.restore_hang_section(&back);
                }
            })
        };
        let whole = run(1_000, false);
        assert_eq!(whole.0, kind);
        assert_eq!(run(7, false), whole, "{kind:?} split every 7 ms");
        assert_eq!(run(7, true), whole, "{kind:?} restored every 7 ms");
    }
}

/// Why the hang clocks are state: a tracker rebuilt empty at every pause starts `since` afresh,
/// so a caller pausing every 30 ms never sees the 50 ms hang.
#[test]
fn a_tracker_rebuilt_empty_at_a_pause_would_hide_the_hang() {
    let mut whole = machine(true);
    short_hang(&mut whole);
    load(&mut whole, &sha_busy_poll(false));
    let (_, _, at, _) = stuck_in_pieces(&mut whole, 1_000, |_| {});
    assert!(at < VTime::from_ms(52));
    let mut m = machine(true);
    short_hang(&mut m);
    load(&mut m, &sha_busy_poll(false));
    for i in 1..=10 {
        assert_eq!(m.run(until_ms(30 * i)).reason, StopReason::Until);
        m.poll = crate::poll_ff::PollTracker::with_hang(m.poll.hang);
    }
}

#[test]
fn hang_and_disabled_models_come_from_the_machine_config() {
    let assets = || {
        Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0)).unwrap()
    };
    let hang = HangCfg {
        enabled: false,
        stuck_ms: 7,
    };
    let cfg = MachineConfig {
        hang,
        disabled_models: vec!["spi2".to_string()],
        ..MachineConfig::default()
    };
    let m = Machine::new(cfg, assets()).unwrap();
    assert_eq!(m.hang_detector(), hang);
    let spi2 = pemu_soc_c3::periph::BLOCKS
        .iter()
        .find(|b| b.name == "spi2")
        .unwrap()
        .id;
    assert!(m.model_disabled(spi2));

    let mut m = machine(true);
    short_hang(&mut m);
    assert!(m.disable_model("spi2"));
    assert!(m.disable_model("spi2"));
    assert!(!m.disable_model("no-such-block"));
    assert_eq!(m.config().hang.stuck_ms, 50);
    assert_eq!(m.config().disabled_models, vec!["spi2".to_string()]);

    let cfg = MachineConfig {
        disabled_models: vec!["no-such-block".to_string()],
        ..MachineConfig::default()
    };
    assert!(matches!(
        Machine::new(cfg, assets()),
        Err(crate::config::ConfigError::UnknownModel(n)) if n == "no-such-block"
    ));
}

#[test]
fn a_disabled_models_store_survives_a_snapshot_and_restore() {
    use pemu_core::snap::SnapOpts;
    let mut m = machine(false);
    assert!(m.disable_model("spi2"));
    load(&mut m, &spi2_update_wait());
    m.run(RunLimits::insns(20));
    let saved = m.disabled.section();
    assert_ne!(
        saved,
        machine(false).disabled.section(),
        "the guest wrote SPI_CMD into the store"
    );
    let snap = m.snapshot(SnapOpts::default());

    let mut fresh = machine(false);
    assert!(fresh.disable_model("spi2"));
    fresh.restore(&snap).expect("same identity");
    assert_eq!(fresh.disabled.section(), saved);
    assert_eq!(fresh.state_hash(), m.state_hash());
    let (a, b) = (m.run(until_ms(5)), fresh.run(until_ms(5)));
    assert_eq!(a.reason, b.reason);
    assert!(matches!(a.reason, StopReason::Stuck(_)), "{:?}", a.reason);

    let mut corrupt = snap.clone();
    let id = pemu_core::snap::SectionId::new(crate::snapshot::SOC_DISABLED);
    let empty = pemu_core::snap::serde_section(
        &crate::disable::DisabledModels::default().section(),
        crate::snapshot::SECTION_VERSION,
        "soc.disabled",
    )
    .unwrap();
    corrupt.sections.insert(id, empty);
    let mut target = machine(false);
    assert!(target.disable_model("spi2"));
    let before = target.state_hash();
    assert!(matches!(
        target.restore(&corrupt),
        Err(pemu_core::snap::SnapError::Malformed {
            at: "soc.disabled",
            ..
        })
    ));
    assert_eq!(target.state_hash(), before);
}

#[test]
fn a_fork_keeps_the_fast_forward_switches_the_hang_detector_and_the_disabled_models() {
    for (poll_ff, rom_ff) in [(true, false), (false, true)] {
        let mut m = machine(poll_ff);
        m.set_rom_delay_ff(rom_ff);
        short_hang(&mut m);
        load(&mut m, &sha_busy_poll(false));
        assert_eq!(m.run(until_ms(20)).reason, StopReason::Until);
        let mut copy = m.fork(pemu_core::snap::LivePolicy::Refuse).unwrap();
        assert_eq!(copy.poll_ff, poll_ff);
        assert_eq!(copy.rom_delay_ff(), rom_ff);
        assert_eq!(copy.hang_detector(), m.hang_detector());
        assert_eq!(copy.hang_section(), m.hang_section().reverified());
        let (a, b) = (m.run(until_ms(1_000)), copy.run(until_ms(1_000)));
        assert!(matches!(a.reason, StopReason::Stuck(_)), "{:?}", a.reason);
        assert_eq!(a.reason, b.reason);
        assert_eq!(a.insns, b.insns);
        assert_eq!(m.state_hash(), copy.state_hash());
    }
    let mut m = machine(true);
    assert!(m.disable_model("spi2"));
    let copy = m.fork(pemu_core::snap::LivePolicy::Refuse).unwrap();
    assert_eq!(copy.config().disabled_models, vec!["spi2".to_string()]);
    assert_eq!(copy.disabled.section(), m.disabled.section());
}

/// A snapshot at any instruction of a hanging poll, fast-forward on or off, restored into a fresh
/// machine, reports the same `Stuck` at the same instruction and time, with the same state hash
/// and tracker.
#[test]
fn a_hang_restored_from_a_snapshot_at_any_instruction_is_reported_where_the_straight_run_reports_it()
 {
    use pemu_core::snap::{SnapOpts, Snapshot};
    for prog in [sha_busy_poll(false), sha_busy_poll(true)] {
        for ff in [true, false] {
            let fresh = || {
                let mut m = machine(ff);
                short_hang(&mut m);
                load(&mut m, &prog);
                m
            };
            let mut straight = fresh();
            let whole = straight.run(until_ms(1_000));
            let StopReason::Stuck(report) = &whole.reason else {
                panic!("expected Stuck, got {:?}", whole.reason);
            };
            let total = straight.hart.insns;
            for k in [1, 64, 65, total / 2, total - 1] {
                let mut m = fresh();
                assert_eq!(m.run(RunLimits::insns(k)).reason, StopReason::MaxInsns);
                let bytes = m.snapshot(SnapOpts::default()).to_bytes().unwrap();
                let mut back = fresh();
                back.restore(&Snapshot::from_bytes(&bytes).unwrap())
                    .expect("same identity");
                assert_eq!(back.hang_section(), m.hang_section().reverified());
                let out = back.run(until_ms(1_000));
                assert_eq!(&out.reason, &whole.reason, "ff {ff}, snapshot at {k}");
                assert!(matches!(&out.reason, StopReason::Stuck(r) if r == report));
                assert_eq!(back.hart.insns, total, "ff {ff}, snapshot at {k}");
                assert_eq!(back.now(), straight.now());
                assert_eq!(back.state_hash(), straight.state_hash());
                assert_eq!(back.hang_section(), straight.hang_section());
            }
        }
    }
}

#[test]
fn observe_hooks_inside_the_rom_delay_loop_count_every_iteration_with_the_shortcut_on_or_off() {
    const HEAD: u32 = 0x4004_7E9E;
    let hooks = [HEAD, HEAD + 4, HEAD + 6];
    let fires = |m: &Machine| hooks.map(|pc| m.observe_fires(pc).map(|(n, _)| n));
    let build = |ff: bool, executor: Executor| {
        let mut m = machine(true);
        m.set_rom_delay_ff(ff);
        m.set_executor(executor);
        load(&mut m, &rom_delay(4_000));
        for pc in hooks {
            assert!(m.add_observe_hook(pc, "delay"));
        }
        m
    };
    let lim = || RunLimits {
        until: None,
        max_insns: Some(200_000),
        stops: StopSet {
            breakpoints: vec![AFTER_DELAY],
            ..StopSet::default()
        },
    };
    let mut results = Vec::new();
    for executor in [Executor::Engine, Executor::Reference] {
        for ff in [true, false] {
            let mut m = build(ff, executor);
            let out = m.run(lim());
            assert_eq!(out.reason, StopReason::Breakpoint(AFTER_DELAY));
            let straight = (fires(&m), m.state_hash(), m.hart.insns);
            assert!(
                straight.0.iter().all(|n| n.is_some_and(|n| n > 100)),
                "{straight:?}"
            );
            // Stopped at odd instruction counts, every other stop restored into a fresh machine.
            let mut m = build(ff, executor);
            let mut step = 0;
            loop {
                step += 1;
                let out = m.run(RunLimits {
                    max_insns: Some(997 + step),
                    ..lim()
                });
                if out.reason != StopReason::MaxInsns {
                    assert_eq!(out.reason, StopReason::Breakpoint(AFTER_DELAY));
                    break;
                }
                if step % 2 == 0 {
                    let snap = m.snapshot(pemu_core::snap::SnapOpts::default());
                    let mut fresh = build(ff, executor);
                    fresh.restore(&snap).expect("restores");
                    m = fresh;
                }
            }
            assert_eq!(
                (fires(&m), m.state_hash(), m.hart.insns),
                straight,
                "{executor:?} ff={ff}: stopped differs from straight"
            );
            results.push(straight);
        }
    }
    assert!(
        results.iter().all(|r| *r == results[0]),
        "the shortcut or the executor changed the fires: {:?}",
        results.iter().map(|r| r.0).collect::<Vec<_>>()
    );
}
