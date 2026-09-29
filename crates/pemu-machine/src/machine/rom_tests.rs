//! Tests over the bundled ROM and a synthesized eFuse, so they need no corpus.

use super::*;
use crate::executor::Executor;
use crate::run::RunLimits;
use crate::stops::{LinePattern, Matcher, MatcherId, StopReason, StopSet, Watch};
use pemu_board::power::RailState;
use pemu_core::hostio::{EventKind, HostEvent, SerialStream};
use pemu_core::input::{ButtonId, SerialChan};
use pemu_core::sched::EventKey;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_rv32::bus::{Access, Bus, HartView};
use pemu_soc_c3::mem::{ROM_DATA_BASE, ROM_DATA_IMAGE_OFF};

fn machine() -> Machine {
    machine_with(MachineConfig::default())
}

fn machine_with(cfg: MachineConfig) -> Machine {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    Machine::new(cfg, assets).expect("the ROM fits the ROM window")
}

/// SRAM1 in its instruction view, where the tests place hand-assembled code.
const PROG: u32 = pemu_soc_c3::mem::SRAM1_IRAM_BASE;

fn load_program(m: &mut Machine, words: &[u32]) {
    for (i, w) in words.iter().enumerate() {
        let stored = m.soc.store_mem(PROG + 4 * i as u32, 4, *w);
        assert!(
            matches!(stored, pemu_soc_c3::Stored::Wrote { .. }),
            "SRAM1 IRAM is writable"
        );
    }
    m.hart.pc = PROG;
}

/// `lui a0, 0x600C5; loop: sw zero, 0(a0); j loop`: a store to MMU entry 0 every second
/// instruction.
const MMU_WRITE_LOOP: [u32; 3] = [0x600C_5537, 0x0005_2023, 0xFFDF_F06F];

const RESET_MATCHER: MatcherId = MatcherId(0xE5);

fn reset_stop() -> StopSet {
    StopSet {
        matchers: vec![(RESET_MATCHER, Matcher::Event(EventKind::Reset))],
        ..StopSet::default()
    }
}

fn last_reset(m: &Machine) -> ResetKind {
    let events: Vec<HostEvent> = m.io.events.slices(0).iter().copied().collect();
    let event = events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::Reset)
        .expect("a reset was recorded");
    let cause = u8::try_from(event.arg).expect("a reset cause is one byte");
    ResetKind::of(ResetCause(cause)).expect("the recorded cause is a documented one")
}

fn until(t: VTime) -> RunLimits {
    RunLimits {
        until: Some(t),
        max_insns: None,
        stops: StopSet::default(),
    }
}

fn rom_word(m: &Machine, off: usize) -> u32 {
    let b = &m.assets.rom.bytes()[off..off + 4];
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

#[test]
fn new_maps_the_rom_into_its_instruction_view_and_its_data_alias() {
    let mut m = machine();
    assert_eq!(m.guest_mem().load(ROM_BASE, 4), Some(rom_word(&m, 0)));
    // The data view aliases image offset 0x40000.
    assert_eq!(
        m.guest_mem().load(ROM_DATA_BASE, 4),
        Some(rom_word(&m, ROM_DATA_IMAGE_OFF as usize))
    );
}

#[test]
fn new_starts_the_hart_at_the_rom_reset_vector_with_time_at_zero() {
    let m = machine();
    assert_eq!(m.hart().pc, ROM_BASE);
    assert_eq!(m.hart().insns, 0);
    assert!(!m.hart().wfi);
    assert_eq!(m.now(), VTime(0));
}

#[test]
fn a_synthesized_efuse_leaves_the_machine_untainted() {
    let m = machine();
    assert!(!m.is_tainted());
    assert!(!MachineApi::is_tainted(&m));
}

/// Taint follows the origin of the image, not any word's value, so re-imported synthesized words
/// prove it without a device MAC, unique id or calibration word.
#[test]
fn the_receipt_of_a_machine_on_an_imported_efuse_dump_says_tainted() {
    let mut synthesized = machine();
    assert!(
        !synthesized.receipt().tainted,
        "a synthesized eFuse is no device's"
    );

    let words: Vec<u8> = EfuseImage::synth(7)
        .dump_words()
        .iter()
        .flat_map(|w| w.to_le_bytes())
        .collect();
    let dump = EfuseImage::from_dump(&words).expect("the synthesized words re-import");
    assert!(dump.tainted(), "an imported dump taints");
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, dump)
        .expect("the bundled ROM is pinned");
    let cfg = MachineConfig {
        efuse: EfuseSource::Dump,
        ..MachineConfig::default()
    };
    let mut imported = Machine::new(cfg, assets).expect("the ROM fits the ROM window");
    assert!(imported.is_tainted());
    assert!(
        imported.receipt().tainted,
        "the taint reaches the receipt, which is the only way out of the machine"
    );
}

#[test]
fn console_bytes_reach_the_host_through_the_models() {
    const UART0_FIFO: u32 = 0x6000_0000;
    const USJ_EP1: u32 = 0x6004_3000;
    const USJ_EP1_CONF: u32 = 0x6004_3004;
    let mut m = machine();
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: 0,
    };
    m.with_bus(|bus, _| {
        bus.store_slow(UART0_FIFO, 4, u32::from(b'O'), &view);
        bus.store_slow(UART0_FIFO + 0x20, 4, 0xFFFF_FFFF, &view);
        bus.store_slow(USJ_EP1, 4, u32::from(b'k'), &view);
        bus.store_slow(USJ_EP1_CONF, 4, 1, &view);
    });
    m.apply_pending_wiring();
    let uart: Vec<u8> = m.io.uart0_tx.slices(0).iter().copied().collect();
    let usj: Vec<u8> = m.io.usj_tx.slices(0).iter().copied().collect();
    assert_eq!(uart, b"O");
    assert_eq!(usj, b"k");
    assert!(m.applied_wiring_by_kind().usj_io > 0);
}

#[test]
fn input_refuses_an_instant_that_has_already_passed() {
    let mut m = machine();
    m.run(RunLimits::insns(1_000));
    let now = m.now();
    assert!(now > VTime(0));
    let err = m
        .input(At::Vt(VTime(0)), InputEvent::Power { down: true })
        .expect_err("an instant in the past cannot be journaled");
    assert_eq!(err, InputError {});
    assert!(m.input(At::Now, InputEvent::Power { down: true }).is_ok());
}

#[test]
fn a_due_board_input_is_applied_and_a_peripheral_input_is_counted() {
    let mut m = machine();
    m.input(
        At::Now,
        InputEvent::Button {
            id: ButtonId::Ok,
            down: true,
        },
    )
    .expect("now is not in the past");
    m.input(At::Now, InputEvent::RtcEpoch { unix_us: 1 })
        .expect("now is not in the past");
    m.run(RunLimits::insns(1_000));
    // The RTC epoch has no consumer yet.
    assert_eq!(m.unapplied_inputs(), 1);
}

#[test]
fn a_due_usj_serial_in_reaches_the_out_endpoint() {
    let mut m = machine();
    m.input(
        At::Now,
        InputEvent::SerialIn {
            chan: SerialChan::USJ,
            data: b"ping".to_vec(),
        },
    )
    .expect("now is not in the past");
    m.run(RunLimits::insns(1_000));
    assert_eq!(m.unapplied_inputs(), 0);
    assert_eq!(m.soc.devices.usj.out_len(), 4, "one packet in the OUT FIFO");
    assert!(m.io().usj_rx.is_empty());
}

#[test]
fn a_uart0_serial_in_is_refused_and_not_counted() {
    let mut m = machine();
    let refused = m.input(
        At::Now,
        InputEvent::SerialIn {
            chan: SerialChan::UART0,
            data: vec![b'x'],
        },
    );
    assert!(refused.is_err());
    m.run(RunLimits::insns(1_000));
    assert_eq!(m.unapplied_inputs(), 0);
    assert!(m.io().usj_rx.is_empty());
}

/// Clears the RWDT flash-boot hold as the bootloader does: the `WDTWPROTECT` key, then
/// `WDTCONFIG0` with `FLASHBOOT_MOD_EN` and `WDT_EN` clear.
fn disarm_flash_boot_hold(m: &mut Machine) {
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
}

/// Unplugs the USB host, which stops the USJ model's 1 kHz SOF tick.
fn detach_usb(m: &mut Machine) {
    let now = m.now();
    m.soc.devices.usj.set_link(
        pemu_soc_c3::periph::usj::HostLink::DETACHED,
        now,
        &mut m.sched,
    );
}

#[test]
fn an_unwakeable_wfi_ends_the_run_as_a_deadlock() {
    let mut m = machine();
    // Remove every wake-up source: the flash-boot hold, the USB SOF tick, and any `until`.
    disarm_flash_boot_hold(&mut m);
    detach_usb(&mut m);
    assert_eq!(
        m.sched.next_time(),
        None,
        "the flash-boot hold is still armed"
    );
    m.hart.wfi = true;
    let out = m.run(RunLimits::insns(1_000));
    assert_eq!(out.reason, StopReason::Deadlock);
    assert_eq!(out.insns, 0);
    assert_eq!(
        out.idle_ps, 0,
        "the run idled {} ps before giving up",
        out.idle_ps
    );
    assert_eq!(out.vt, VTime(0));
}

#[test]
fn an_idle_hart_idles_to_the_until_limit_rather_than_deadlocking() {
    let mut m = machine();
    // A watchdog stage resets the chip without an interrupt, so a hart waiting on it is not dead.
    m.hart.wfi = true;
    let target = VTime(1_000_000);
    let out = m.run(until(target));
    assert_eq!(out.reason, StopReason::Until);
    assert!(out.vt >= target, "it idled only to {:?}", out.vt);
    assert_eq!(out.insns, 0);
    assert!(out.idle_ps >= target.0);
}

#[test]
fn a_power_hold_reaches_its_deadline_through_the_scheduler_and_raises_the_rail() {
    // `off_hold_ms` shortened to 1 ms (2000 ms would be 80 million instructions).
    let mut cfg = MachineConfig::default();
    cfg.board.power.off_hold_ms = 1;
    let mut m = machine_with(cfg);
    assert_eq!(m.board.power.state(), RailState::On);

    m.input(At::Now, InputEvent::Power { down: true })
        .expect("now is not in the past");
    m.run(RunLimits::insns(1));
    // The press alone is not the edge: the board's wake-up at `PowerRail::deadline` must fire.
    assert_eq!(m.board.power.state(), RailState::On);
    assert!(
        m.sched
            .pending()
            .iter()
            .any(|(t, _, k)| k.owner == Owner::Chip(BOARD_CHIP) && *t == VTime::from_ms(1)),
        "the power-button hold deadline was dropped instead of scheduled"
    );

    let out = m.run(until(VTime::from_ms(2)));
    assert_eq!(out.reason, StopReason::Until);
    assert_eq!(m.board.power.state(), RailState::Off);
    assert!(!m.mcu_powered());
    assert_eq!(m.pending_board_effects(), 0);
    assert_eq!(m.pending_board_events(), 0);
    let mut last = [HostEvent::default()];
    m.io.events.read(m.io.events.head() - 1, &mut last);
    assert_eq!((last[0].kind, last[0].arg), (EventKind::Power, 0));
    let pending = m.sched.pending();
    assert!(
        pending
            .iter()
            .all(|(_, _, k)| k.owner != Owner::Periph(pemu_soc_c3::periph::id::USJ)),
        "the SOF tick kept running with the rail off: {pending:?}"
    );
    let insns = m.hart.insns;
    let out = m.run(until(VTime::from_ms(10)));
    assert_eq!(out.reason, StopReason::Until);
    assert_eq!(m.hart.insns, insns);
    assert_eq!(m.undispatched_events(), 0);
}

#[test]
fn a_rail_cycle_clears_sram_and_powers_on_with_a_power_on_reset() {
    let mut cfg = MachineConfig::default();
    cfg.board.power.off_hold_ms = 1;
    cfg.board.power.on_hold_ms = 1;
    let mut m = machine_with(cfg);
    load_program(&mut m, &[0x0000_006F]);
    let resets = m.resets();
    let press = |m: &mut Machine, at: u64| {
        m.input(At::Vt(VTime::from_ms(at)), InputEvent::Power { down: true })
            .expect("a future instant");
        m.input(
            At::Vt(VTime::from_ms(at + 2)),
            InputEvent::Power { down: false },
        )
        .expect("a future instant");
    };
    press(&mut m, 1);
    m.run(until(VTime::from_ms(4)));
    assert!(!m.mcu_powered());
    assert_eq!(
        m.guest_mem().load(PROG, 4),
        Some(0),
        "SRAM kept its contents"
    );

    press(&mut m, 5);
    let out = m.run(RunLimits {
        stops: reset_stop(),
        ..until(VTime::from_ms(10))
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(RESET_MATCHER),
        "the rail coming up did not reset the chip"
    );
    let kind = last_reset(&m);
    assert_eq!(kind.cause, ResetCause::POWERON);
    assert!(m.mcu_powered());
    assert_eq!(m.resets(), resets + 1);
    assert_eq!(m.hart.pc, ROM_BASE);
    let mut usj = String::new();
    for _ in 0..20 {
        m.run(RunLimits::insns(10_000));
        let ring = m.io.serial_ring(SerialStream::UsjTx);
        usj = String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>())
            .into_owned();
        if usj.contains("boot:") {
            break;
        }
    }
    assert!(usj.contains("rst:0x1 (POWERON)"), "{usj:?}");
}

#[test]
fn an_efuse_configuration_that_disagrees_with_its_image_is_refused() {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let cfg = MachineConfig {
        efuse: EfuseSource::Dump,
        ..MachineConfig::default()
    };
    let err = match Machine::new(cfg, assets) {
        Ok(_) => panic!("a synthesized image is not a dump"),
        Err(e) => e,
    };
    assert_eq!(
        err,
        ConfigError::EfuseMismatch {
            config: EfuseSource::Dump,
            tainted: false,
        }
    );
}

#[test]
fn an_event_for_an_id_no_block_claims_is_counted_rather_than_dropped() {
    use pemu_core::sched::PeriphId;

    let mut m = machine();
    let now = m.now();
    // No `c3_devices!` row has this id: it must not pass for a delivery.
    m.sched.schedule(
        now,
        now,
        EventKey {
            owner: Owner::Periph(PeriphId(9_999)),
            tag: 3,
        },
    );
    m.run(RunLimits::insns(1));
    assert_eq!(m.undispatched_events(), 1);
    assert_eq!(m.unapplied_wiring(), 0);
}

#[test]
fn mmu_entry_writes_are_applied_under_their_own_kind() {
    let mut m = machine();
    load_program(&mut m, &MMU_WRITE_LOOP);
    let before = m.applied_wiring_by_kind();
    let out = m.run(RunLimits::insns(101));
    assert_eq!(out.insns, 101, "the engine's budget is exact");
    let kinds = m.applied_wiring_by_kind();
    assert_eq!(kinds.mmu_entry - before.mmu_entry, 50, "{kinds:?}");
    assert_eq!(m.unapplied_wiring(), 0);
}

/// A hot wiring register (StackGuard's `SpMonitor`) cannot pile effects up in the SoC.
#[test]
fn wiring_effects_are_taken_after_each_access_that_raises_one() {
    let mut m = machine();
    load_program(&mut m, &MMU_WRITE_LOOP);
    let before = m.applied_wiring_by_kind().mmu_entry;
    m.run(RunLimits::insns(101));
    assert_eq!(m.applied_wiring_by_kind().mmu_entry - before, 50);
    assert_eq!(
        m.peak_pending_wiring, 1,
        "effects waited in the SoC until the run call ended"
    );
}

/// `lui a0, 0x60008; lui a1, 0x80000; sw a1, 0(a0); j .`: ROM `software_reset`'s write of
/// `RTC_CNTL_OPTIONS0.SW_SYS_RST` (bit 31 at 0x6000_8000), then a self-loop.
const SW_SYS_RST: [u32; 4] = [0x6000_8537, 0x8000_05B7, 0x00B5_2023, 0x0000_006F];

#[test]
fn a_guest_software_reset_is_sequenced_and_the_rom_boots_again() {
    let mut m = machine();
    load_program(&mut m, &SW_SYS_RST);
    let out = m.run(RunLimits {
        stops: reset_stop(),
        ..RunLimits::insns(1_000)
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(RESET_MATCHER),
        "the reset stop did not fire"
    );
    let kind = last_reset(&m);
    assert_eq!(kind.cause, ResetCause::RTC_SW_SYS);
    assert_eq!(out.insns, 3, "the store is the third instruction");
    assert_eq!(m.hart.pc, ROM_BASE);
    assert_eq!(m.applied_wiring_by_kind().chip_reset, 1);
    assert_eq!(m.unapplied_wiring_by_kind().chip_reset, 0);
    let mut usj = String::new();
    for _ in 0..40 {
        m.run(RunLimits::insns(10_000));
        let ring = m.io.serial_ring(SerialStream::UsjTx);
        usj = String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>())
            .into_owned();
        if usj.contains("boot:") {
            break;
        }
    }
    assert!(
        usj.contains("rst:0x3 (RTC_SW_SYS_RST),boot:0xa (SPI_FAST_FLASH_BOOT)"),
        "console after the reset: {usj:?}"
    );
}

/// The hold fires about 2.94 s after power-on.
#[test]
fn a_run_past_the_rwdt_flash_boot_hold_resets_and_prints_its_cause() {
    let mut m = machine();
    m.hart.wfi = true;
    let out = m.run(RunLimits {
        stops: reset_stop(),
        ..until(VTime::from_ms(3_000))
    });
    assert_eq!(
        out.reason,
        StopReason::Matcher(RESET_MATCHER),
        "the RWDT hold did not reset the chip"
    );
    let kind = last_reset(&m);
    assert!(kind.fanout.reaches_all_blocks(), "{kind:?}");
    assert!(
        out.vt > VTime::from_ms(2_900) && out.vt < VTime::from_ms(3_000),
        "the hold is about 2.94 s, not {:?}",
        out.vt
    );
    assert_eq!(m.applied_wiring_by_kind().chip_reset, 1);
    assert!(!m.hart.wfi, "the reset took the hart out of wfi");
    let mut usj = String::new();
    for _ in 0..40 {
        m.run(RunLimits::insns(10_000));
        let ring = m.io.serial_ring(SerialStream::UsjTx);
        usj = String::from_utf8_lossy(&ring.slices(0).iter().copied().collect::<Vec<u8>>())
            .into_owned();
        if usj.contains("boot:") {
            break;
        }
    }
    let want = format!("rst:0x{:x} ", kind.cause.0);
    assert!(
        usj.contains(&want),
        "the ROM did not print the RWDT cause {want:?}: {usj:?}"
    );
}

/// `addi a0, a0, 1` four times, then `j .`.
const COUNT_FOUR: [u32; 5] = [
    0x0015_0513,
    0x0015_0513,
    0x0015_0513,
    0x0015_0513,
    0x0000_006F,
];

/// Under both executors (`g3-behavior g3-debug-stops`).
#[test]
fn a_breakpoint_stops_before_its_pc_and_resumes_past_it_once() {
    for executor in [Executor::Engine, Executor::Reference] {
        let mut m = machine();
        m.set_executor(executor);
        load_program(&mut m, &COUNT_FOUR);
        let bp = || RunLimits {
            stops: StopSet {
                breakpoints: vec![PROG + 8],
                ..StopSet::default()
            },
            ..RunLimits::insns(100)
        };
        let out = m.run(bp());
        assert_eq!(out.reason, StopReason::Breakpoint(PROG + 8), "{executor:?}");
        assert_eq!((out.insns, m.hart.x[10], m.hart.pc), (2, 2, PROG + 8));
        let out = m.run(bp());
        assert_eq!(out.reason, StopReason::MaxInsns, "{executor:?}");
        assert_eq!(out.insns, 100);
        assert_eq!(
            m.hart.x[10], 4,
            "{executor:?}: the resumed instruction ran once"
        );
        assert_eq!(
            m.resume_breakpoint, None,
            "the marker does not remain in the state"
        );
    }
}

#[test]
fn a_foreign_hook_is_left_alone_and_a_breakpoint_on_its_pc_still_stops() {
    use pemu_rv32::engine::HookId;
    // Class 6 is no `pemu_hle::hooks::HookKind`, so no binding produced this id.
    const FOREIGN: HookId = HookId(6 << 28);
    let mut m = machine();
    load_program(&mut m, &COUNT_FOUR);
    m.hooks.insert(PROG + 8, FOREIGN);
    let out = m.run(RunLimits::insns(100));
    assert_eq!(
        out.reason,
        StopReason::MaxInsns,
        "a foreign hook is not a stop"
    );
    assert_eq!(m.hart.x[10], 4, "the hooked instruction ran once");
    assert_eq!(
        m.hooks.get(PROG + 8),
        Some(FOREIGN),
        "the run left the hook bound"
    );

    let mut m = machine();
    load_program(&mut m, &COUNT_FOUR);
    m.hooks.insert(PROG + 8, FOREIGN);
    let bp = || RunLimits {
        stops: StopSet {
            breakpoints: vec![PROG + 8],
            ..StopSet::default()
        },
        ..RunLimits::insns(100)
    };
    let out = m.run(bp());
    assert_eq!(out.reason, StopReason::Breakpoint(PROG + 8));
    assert_eq!(m.hooks.get(PROG + 8), Some(FOREIGN));
    let out = m.run(RunLimits::insns(100));
    assert_eq!(out.reason, StopReason::MaxInsns);
    assert_eq!(m.hart.x[10], 4);
    assert_eq!(
        m.hooks.get(PROG + 8),
        Some(FOREIGN),
        "a run without the breakpoint does not remove a hook it did not install"
    );
}

/// `lui a1, 0x3FC82; addi a0, zero, 7; sw a0, 0(a1); j .`: a store through the DRAM view.
const STORE_DRAM: [u32; 4] = [0x3FC8_25B7, 0x0070_0513, 0x00A5_A023, 0x0000_006F];

#[test]
fn a_write_watch_matches_every_view_and_stops_after_the_writer() {
    let mut m = machine();
    load_program(&mut m, &STORE_DRAM);
    let out = m.run(RunLimits {
        stops: StopSet {
            watches: vec![Watch {
                addr: 0x4038_2000,
                len: 4,
            }],
            ..StopSet::default()
        },
        ..RunLimits::insns(1_000)
    });
    assert_eq!(
        out.reason,
        StopReason::Watchpoint {
            addr: 0x3FC8_2000,
            pc: PROG + 8
        }
    );
    assert_eq!(out.insns, 3);
    assert_eq!(m.guest_mem().load(0x4038_2000, 4), Some(7));
    assert!(
        !m.soc.is_slow_marked(0x3FC8_2000) && !m.soc.is_slow_marked(0x4038_2000),
        "the run left its PF_SLOW marks behind"
    );
}

#[test]
fn a_rom_store_is_not_a_watch_hit() {
    const STORE_ROM: [u32; 3] = [0x4000_05B7, 0x00A5_A023, 0x0000_006F];
    let stops = StopSet {
        watches: vec![Watch {
            addr: ROM_BASE,
            len: 4,
        }],
        ..StopSet::default()
    };
    assert!(stops.check().is_err());
    let mut m = machine();
    load_program(&mut m, &STORE_ROM);
    let out = m.run(RunLimits {
        stops,
        ..RunLimits::insns(100)
    });
    assert_eq!(out.reason, StopReason::MaxInsns);
    assert_eq!(m.guest_mem().load(ROM_BASE, 4), Some(rom_word(&m, 0)));
}

/// `lui a1, 0x60000`, then `h`, `i`, `\n` and `x` written to `UART_FIFO`, then `j .`.
const UART_HI: [u32; 10] = [
    0x6000_05B7,
    0x0680_0513,
    0x00A5_A023,
    0x0690_0513,
    0x00A5_A023,
    0x00A0_0513,
    0x00A5_A023,
    0x0780_0513,
    0x00A5_A023,
    0x0000_006F,
];

#[test]
fn a_serial_matcher_stops_at_the_instruction_that_ends_the_line() {
    let mut m = machine();
    load_program(&mut m, &UART_HI);
    let out = m.run(RunLimits {
        stops: StopSet {
            matchers: vec![(
                MatcherId(3),
                Matcher::Serial {
                    stream: SerialStream::Uart0Tx,
                    pattern: LinePattern::Exact("hi".into()),
                },
            )],
            ..StopSet::default()
        },
        ..RunLimits::insns(1_000)
    });
    assert_eq!(out.reason, StopReason::Matcher(MatcherId(3)));
    assert_eq!(out.insns, 7);
    let uart: Vec<u8> = m.io.uart0_tx.slices(0).iter().copied().collect();
    assert_eq!(uart, b"hi\n");
}

#[test]
fn an_event_matcher_fires_on_the_reset_record() {
    let mut m = machine();
    load_program(&mut m, &SW_SYS_RST);
    let out = m.run(RunLimits {
        stops: StopSet {
            matchers: vec![(MatcherId(9), Matcher::Event(EventKind::Reset))],
            ..StopSet::default()
        },
        ..RunLimits::insns(1_000)
    });
    assert_eq!(out.reason, StopReason::Matcher(MatcherId(9)));
    assert_eq!((out.insns, m.hart.pc), (3, ROM_BASE));
}

/// A flat encoding of any `Serialize` value (scalars little-endian, lengths and variant indices
/// as `u64`), for comparing two states of the same types.
#[derive(Default)]
struct Flat(Vec<u8>);

#[derive(Debug)]
struct FlatError(String);

impl std::fmt::Display for FlatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FlatError {}

impl pemu_core::serde::ser::Error for FlatError {
    fn custom<T: std::fmt::Display>(msg: T) -> Self {
        FlatError(msg.to_string())
    }
}

fn flat<T: pemu_core::serde::Serialize + ?Sized>(value: &T) -> Vec<u8> {
    let mut out = Flat::default();
    value.serialize(&mut out).expect("model state serializes");
    out.0
}

impl Flat {
    fn put(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn len(&mut self, n: usize) {
        self.put(&(n as u64).to_le_bytes());
    }
}

macro_rules! flat_scalars {
    ($($method:ident: $ty:ty),*) => {
        $(
            fn $method(self, v: $ty) -> Result<(), FlatError> {
                self.put(&v.to_le_bytes());
                Ok(())
            }
        )*
    };
}

impl pemu_core::serde::Serializer for &mut Flat {
    type Ok = ();
    type Error = FlatError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    flat_scalars!(
        serialize_i8: i8, serialize_i16: i16, serialize_i32: i32, serialize_i64: i64,
        serialize_i128: i128, serialize_u8: u8, serialize_u16: u16, serialize_u32: u32,
        serialize_u64: u64, serialize_u128: u128, serialize_f32: f32, serialize_f64: f64
    );

    fn serialize_bool(self, v: bool) -> Result<(), FlatError> {
        self.put(&[u8::from(v)]);
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<(), FlatError> {
        self.put(&u32::from(v).to_le_bytes());
        Ok(())
    }
    fn serialize_str(self, v: &str) -> Result<(), FlatError> {
        self.serialize_bytes(v.as_bytes())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), FlatError> {
        self.len(v.len());
        self.put(v);
        Ok(())
    }
    fn serialize_none(self) -> Result<(), FlatError> {
        self.put(&[0]);
        Ok(())
    }
    fn serialize_some<T: pemu_core::serde::Serialize + ?Sized>(
        self,
        v: &T,
    ) -> Result<(), FlatError> {
        self.put(&[1]);
        v.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), FlatError> {
        Ok(())
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<(), FlatError> {
        Ok(())
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
    ) -> Result<(), FlatError> {
        self.len(index as usize);
        Ok(())
    }
    fn serialize_newtype_struct<T: pemu_core::serde::Serialize + ?Sized>(
        self,
        _: &'static str,
        v: &T,
    ) -> Result<(), FlatError> {
        v.serialize(self)
    }
    fn serialize_newtype_variant<T: pemu_core::serde::Serialize + ?Sized>(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        v: &T,
    ) -> Result<(), FlatError> {
        self.len(index as usize);
        v.serialize(self)
    }
    fn serialize_seq(self, n: Option<usize>) -> Result<Self, FlatError> {
        self.len(n.unwrap_or(usize::MAX));
        Ok(self)
    }
    fn serialize_tuple(self, _: usize) -> Result<Self, FlatError> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _: &'static str, _: usize) -> Result<Self, FlatError> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, FlatError> {
        self.len(index as usize);
        Ok(self)
    }
    fn serialize_map(self, n: Option<usize>) -> Result<Self, FlatError> {
        self.len(n.unwrap_or(usize::MAX));
        Ok(self)
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self, FlatError> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        index: u32,
        _: &'static str,
        _: usize,
    ) -> Result<Self, FlatError> {
        self.len(index as usize);
        Ok(self)
    }
}

macro_rules! flat_compound {
    ($($tr:ident :: $method:ident),*) => {
        $(
            impl pemu_core::serde::ser::$tr for &mut Flat {
                type Ok = ();
                type Error = FlatError;
                fn $method<T: pemu_core::serde::Serialize + ?Sized>(
                    &mut self,
                    v: &T,
                ) -> Result<(), FlatError> {
                    v.serialize(&mut **self)
                }
                fn end(self) -> Result<(), FlatError> {
                    Ok(())
                }
            }
        )*
    };
}

flat_compound!(
    SerializeSeq::serialize_element,
    SerializeTuple::serialize_element,
    SerializeTupleStruct::serialize_field,
    SerializeTupleVariant::serialize_field
);

impl pemu_core::serde::ser::SerializeMap for &mut Flat {
    type Ok = ();
    type Error = FlatError;
    fn serialize_key<T: pemu_core::serde::Serialize + ?Sized>(
        &mut self,
        k: &T,
    ) -> Result<(), FlatError> {
        k.serialize(&mut **self)
    }
    fn serialize_value<T: pemu_core::serde::Serialize + ?Sized>(
        &mut self,
        v: &T,
    ) -> Result<(), FlatError> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), FlatError> {
        Ok(())
    }
}

impl pemu_core::serde::ser::SerializeStruct for &mut Flat {
    type Ok = ();
    type Error = FlatError;
    fn serialize_field<T: pemu_core::serde::Serialize + ?Sized>(
        &mut self,
        _: &'static str,
        v: &T,
    ) -> Result<(), FlatError> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), FlatError> {
        Ok(())
    }
}

impl pemu_core::serde::ser::SerializeStructVariant for &mut Flat {
    type Ok = ();
    type Error = FlatError;
    fn serialize_field<T: pemu_core::serde::Serialize + ?Sized>(
        &mut self,
        _: &'static str,
        v: &T,
    ) -> Result<(), FlatError> {
        v.serialize(&mut **self)
    }
    fn end(self) -> Result<(), FlatError> {
        Ok(())
    }
}

/// FNV-1a over `bytes`, so a mismatch report names a digest rather than megabytes.
fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01B3)
    })
}

struct DeviceDigests(Vec<(u32, u64)>);

impl pemu_soc_c3::periph::DeviceVisitor for DeviceDigests {
    fn visit<P: pemu_soc_c3::periph::Peripheral>(&mut self, dev: &mut P) {
        self.0.push((P::BASE, digest(&flat(dev))));
    }
}

/// The whole observable state of a machine, for comparing two executors. Translated blocks and
/// `PF_CODE` marks are executor state, not guest state, so they are left out.
fn full_state(m: &mut Machine) -> Vec<(&'static str, String)> {
    let h = &m.hart;
    let c = &h.csr;
    let mut devices = DeviceDigests(Vec::new());
    m.soc.devices.visit_all(&mut devices);
    let usj: Vec<u8> = m.io.usj_tx.slices(0).iter().copied().collect();
    let uart: Vec<u8> = m.io.uart0_tx.slices(0).iter().copied().collect();
    vec![
        ("x", format!("{:?}", h.x)),
        (
            "pc insns stores wfi",
            format!("{:#x} {} {} {}", h.pc, h.insns, h.stores, h.wfi),
        ),
        ("spmon", format!("{:?}", flat(&h.spmon))),
        (
            "csr",
            format!(
                "{:?}",
                (
                    (c.mstatus, c.mtvec, c.mepc, c.mcause, c.mtval, c.mscratch),
                    (
                        c.pmpcfg, c.pmpaddr, c.tselect, c.tdata1, c.tdata2, c.tcontrol
                    ),
                    (c.mpcer, c.mpcmr, c.csr000),
                )
            ),
        ),
        ("arena", format!("{:#x}", digest(m.soc.arena.bytes()))),
        ("devices", format!("{:x?}", devices.0)),
        (
            "irq",
            format!(
                "{:x?} eip {:#x} epoch {}",
                (0..0x800)
                    .step_by(4)
                    .map(|off| m.irq.read(off))
                    .collect::<Vec<_>>(),
                m.irq.eip(),
                m.irq.epoch()
            ),
        ),
        ("sched", format!("{:?}", m.sched.pending())),
        ("clock", format!("{:?}", m.clock)),
        ("rng", format!("{:#x}", digest(&flat(&m.rng)))),
        ("ledger", format!("{:?}", m.ledger)),
        ("usj", String::from_utf8_lossy(&usj).into_owned()),
        ("uart0", String::from_utf8_lossy(&uart).into_owned()),
        ("now", format!("{:?}", m.now())),
    ]
}

fn assert_same_state(engine: &[(&str, String)], reference: &[(&str, String)]) {
    for ((part, e), (_, r)) in engine.iter().zip(reference) {
        assert_eq!(e, r, "engine and reference differ in {part}");
    }
}

#[test]
fn the_engine_and_the_reference_interpreter_boot_the_rom_identically() {
    let run = |executor| {
        let mut m = machine();
        m.set_executor(executor);
        let out = m.run(RunLimits::insns(300_000));
        assert_eq!(out.reason, StopReason::MaxInsns);
        full_state(&mut m)
    };
    let engine = run(Executor::Engine);
    let reference = run(Executor::Reference);
    let usj = &engine.iter().find(|(p, _)| *p == "usj").expect("usj").1;
    assert!(
        usj.contains("invalid header"),
        "the run reaches the flash boot"
    );
    assert_same_state(&engine, &reference);
}

/// `csrsi mstatus, 8; loop: addi a0, a0, 1; j loop`.
const COUNT_WITH_MIE: [u32; 3] = [0x3004_6073, 0x0015_0513, 0xFFDF_F06F];

/// A line 1 handler: `addi a1, a1, 1; lui t2, 0x60023; addi t3, zero, 1;
/// sw t3, 0x6C(t2)` (SYSTIMER `INT_CLR`), `mret`.
const COUNT_ALARMS: [u32; 5] = [
    0x0015_8593,
    0x6002_33B7,
    0x0010_0E13,
    0x07C3_A623,
    0x3020_0073,
];

#[test]
fn the_engine_and_the_reference_interpreter_agree_on_an_interrupt_driven_program() {
    let run = |executor| {
        let mut m = machine();
        m.set_executor(executor);
        disarm_flash_boot_hold(&mut m);
        load_program(&mut m, &COUNT_WITH_MIE);
        let vector = PROG + 0x1000;
        for (i, w) in COUNT_ALARMS.iter().enumerate() {
            let stored = m.soc.store_mem(vector + 4 + 4 * i as u32, 4, *w);
            assert!(matches!(stored, pemu_soc_c3::Stored::Wrote { .. }));
        }
        route_systimer_to_line_1(&mut m);
        arm_periodic_systimer(&mut m, 1_600);
        m.hart.csr.mtvec = vector | 1;
        let out = m.run(RunLimits::insns(300_000));
        assert_eq!(out.reason, StopReason::MaxInsns);
        let alarms = m.hart.x[11];
        (alarms, full_state(&mut m))
    };
    let (engine_alarms, engine) = run(Executor::Engine);
    let (reference_alarms, reference) = run(Executor::Reference);
    assert!(engine_alarms > 10, "the handler ran {engine_alarms} times");
    assert_eq!(engine_alarms, reference_alarms);
    assert_same_state(&engine, &reference);
}

/// `auipc a1, 0; lw a2, 40(a1); t: addi a0, a0, 1; addi t0, t0, 1; addi t1, zero, 2;
/// bne t0, t1, skip; sw a2, 8(a1); skip: addi t1, zero, 4; blt t0, t1, t; j .`, then the word
/// `addi a0, a0, 10`: on its second turn the loop rewrites its own first instruction, which
/// the block at `t` already holds, with no `fence.i`.
const SELF_MODIFYING: [u32; 11] = [
    0x0000_0597,
    0x0285_A603,
    0x0015_0513,
    0x0012_8293,
    0x0020_0313,
    0x0062_9463,
    0x00C5_A423,
    0x0040_0313,
    0xFE62_C4E3,
    0x0000_006F,
    0x00A5_0513,
];

/// The executor is driven directly with one large budget: the run loop's slices are shorter than
/// the program and would hide a block left stale inside one call.
#[test]
fn self_modifying_code_without_fence_i_runs_the_new_instruction() {
    const INSNS: u64 = 1_000;
    for executor in [Executor::Engine, Executor::Reference] {
        let mut m = machine();
        m.set_executor(executor);
        load_program(&mut m, &SELF_MODIFYING);
        let start = m.hart.insns;
        for _ in 0..2 * INSNS {
            let done = m.hart.insns - start;
            if done >= INSNS {
                break;
            }
            m.execute(INSNS - done);
        }
        assert_eq!(m.hart.insns - start, INSNS, "{executor:?}");
        assert_eq!(m.hart.x[10], 22, "{executor:?}");
        assert_eq!(m.hart.pc, PROG + 36, "{executor:?}");
    }
}

/// Routes SYSTIMER alarm 0 to CPU line 1, enabled, priority 1 over threshold 1.
fn route_systimer_to_line_1(m: &mut Machine) {
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| {
        const INTC: u32 = 0x600C_2000;
        bus.store_slow(INTC + 4 * 37, 4, 1, &view); // SYSTIMER_TARGET0 on line 1
        bus.store_slow(INTC + 0x104, 4, 1 << 1, &view); // CPU_INT_ENABLE
        bus.store_slow(INTC + 0x118, 4, 1, &view); // CPU_INT_PRI_1
        bus.store_slow(INTC + 0x194, 4, 1, &view); // CPU_INT_THRESH
    });
}

/// `lui t0, 10; loop: addi t0, t0, -1; bnez t0, loop; csrsi mstatus, 8; count: addi a0, a0,
/// 1; j count`: 40,960 turns with MIE clear, then MIE set and a counter in `a0`.
const ENABLE_LATE: [u32; 6] = [
    0x0000_A2B7,
    0xFFF2_8293,
    0xFE02_9EE3,
    0x3004_6073,
    0x0015_0513,
    0xFFDF_F06F,
];

/// `lui t0, 10; loop: addi t0, t0, -1; bnez t0, loop; lui a1, 0x60023; addi a2, zero, 1;
/// sw a2, 0x64(a1); count: addi a0, a0, 1; j count`: the same wait, then SYSTIMER `INT_ENA`
/// set through MMIO with MIE already set.
const ENABLE_BY_MMIO: [u32; 8] = [
    0x0000_A2B7,
    0xFFF2_8293,
    0xFE02_9EE3,
    0x6002_35B7,
    0x0010_0613,
    0x06C5_A223,
    0x0015_0513,
    0xFFDF_F06F,
];

/// The alarm fires about 40,000 instructions in and the next 40,000 later, so a run that took the
/// interrupt at the end of its slice would have counted in `a0` first.
#[test]
fn a_pending_interrupt_is_taken_after_the_instruction_that_enables_it() {
    for (program, mmio) in [(&ENABLE_LATE[..], false), (&ENABLE_BY_MMIO[..], true)] {
        interrupt_timing_is_exact(program, mmio);
    }
}

fn interrupt_timing_is_exact(program: &[u32], mmio: bool) {
    #[derive(Debug)]
    enum Variant {
        Engine,
        Reference,
        Watch,
        SerialMatcher,
        Chunks,
    }
    let run = |variant: &Variant| {
        let mut m = machine();
        disarm_flash_boot_hold(&mut m);
        load_program(&mut m, program);
        let vector = PROG + 0x1000;
        let stored = m.soc.store_mem(vector + 4, 4, 0x0000_006F);
        assert!(matches!(stored, pemu_soc_c3::Stored::Wrote { .. }));
        route_systimer_to_line_1(&mut m);
        arm_periodic_systimer(&mut m, 16_000);
        if mmio {
            let view = HartView {
                insns: 0,
                extra: 0,
                pc: PROG,
            };
            m.with_bus(|bus, _| bus.store_slow(0x6002_3064, 4, 0, &view));
            m.hart.csr.mstatus |= pemu_rv32::csr::MSTATUS_MIE;
        }
        m.hart.csr.mtvec = vector | 1;
        let mut stops = StopSet::default();
        match variant {
            Variant::Reference => m.set_executor(Executor::Reference),
            Variant::Watch => stops.watches.push(Watch {
                addr: 0x3FC8_8000,
                len: 4,
            }),
            Variant::SerialMatcher => stops.matchers.push((
                MatcherId(1),
                Matcher::Serial {
                    stream: SerialStream::Uart0Tx,
                    pattern: LinePattern::Contains("never".into()),
                },
            )),
            Variant::Engine | Variant::Chunks => {}
        }
        const TOTAL: u64 = 150_000;
        let chunk = if matches!(variant, Variant::Chunks) {
            7_777
        } else {
            TOTAL
        };
        let mut left = TOTAL;
        while left > 0 {
            let out = m.run(RunLimits {
                stops: stops.clone(),
                ..RunLimits::insns(left.min(chunk))
            });
            assert_eq!(out.reason, StopReason::MaxInsns, "{variant:?}");
            left -= out.insns;
        }
        let h = &m.hart;
        (
            h.csr.mcause,
            h.csr.mepc,
            h.x,
            h.pc,
            h.insns,
            h.csr.mstatus,
            m.now(),
        )
    };
    let engine = run(&Variant::Engine);
    assert_eq!(engine.0, 0x8000_0001, "the alarm interrupt was taken");
    assert_eq!(
        (engine.1, engine.2[10]),
        (PROG + 4 * (program.len() as u32 - 2), 0),
        "taken right after the enabling instruction, before the first count"
    );
    for variant in [
        Variant::Reference,
        Variant::Watch,
        Variant::SerialMatcher,
        Variant::Chunks,
    ] {
        assert_eq!(run(&variant), engine, "{variant:?}");
    }
}

#[test]
fn a_routed_enabled_source_interrupts_the_hart() {
    let mut m = machine();
    disarm_flash_boot_hold(&mut m);
    load_program(&mut m, &[0x0000_006F]);
    let vector = PROG + 0x1000;
    let stored = m.soc.store_mem(vector + 4, 4, 0x0000_006F);
    assert!(matches!(stored, pemu_soc_c3::Stored::Wrote { .. }));
    route_systimer_to_line_1(&mut m);
    arm_periodic_systimer(&mut m, 16_000);
    m.hart.csr.mtvec = vector | 1;
    m.hart.csr.mstatus |= pemu_rv32::csr::MSTATUS_MIE;
    m.run(RunLimits::insns(100_000));
    assert_eq!(m.hart.csr.mcause, 0x8000_0001, "no interrupt was taken");
    assert_eq!(m.hart.csr.mepc, PROG);
    assert_eq!(m.hart.pc, vector + 4);
    assert!(!m.hart.csr.mstatus_mie());
}

#[test]
fn power_on_hands_the_efuse_adc_calibration_to_the_saradc_model() {
    use pemu_loader::efuse_image::field;
    assert_eq!(
        machine().soc.devices.saradc.calibration(),
        AdcCal::default()
    );
    let mut efuse = EfuseImage::synth(0);
    // 0x2A5: sign bit 9 set, magnitude 0xA5, so both words carry bits of it.
    efuse.set(field::ADC1_CAL_VOL_ATTEN[3], 0x2A5);
    let m = machine_with_efuse(efuse);
    assert_eq!(
        m.soc.devices.saradc.calibration(),
        AdcCal {
            blk_version_major: 1,
            cal_vol_atten3: 0x2A5,
        }
    );
}

fn machine_with_efuse(efuse: EfuseImage) -> Machine {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, efuse)
        .expect("the bundled ROM is pinned");
    Machine::new(MachineConfig::default(), assets).expect("the ROM fits the ROM window")
}

fn rwdt_due(m: &Machine) -> VTime {
    m.sched
        .pending()
        .iter()
        .find(|(_, _, k)| k.owner == Owner::Periph(pemu_soc_c3::periph::id::RTC_CNTL))
        .map(|(t, _, _)| *t)
        .expect("power-on arms the RWDT flash-boot hold")
}

/// `EFUSE_RD_MAC_SPI_SYS_3` bits 18 to 20 carry `WAFER_VERSION_MINOR_LO`, 1 for wafer v1.1.
#[test]
fn power_on_loads_the_efuse_image_into_the_register_window() {
    let efuse = EfuseImage::synth(0);
    let word = efuse.dump_words()[6 + 3];
    let mut m = machine_with_efuse(efuse);
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    let read = m.with_bus(|bus, _| bus.load_slow(0x6000_8850, 4, &view));
    let Access::Ok(got) = read else {
        panic!("EFUSE_RD_MAC_SPI_SYS_3 is a plain register read");
    };
    assert_ne!(word, 0, "the synthesized image carries a wafer revision");
    assert_eq!(got, word, "the guest reads the reset value, not the image");
    assert_eq!((got >> 18) & 0x7, 1, "wafer v1.1 minor, low bits");
}

#[test]
fn a_nonzero_wdt_delay_sel_lengthens_the_flash_boot_hold() {
    let plain = rwdt_due(&machine());
    let mut efuse = EfuseImage::synth(0);
    efuse.set(pemu_loader::efuse_image::field::at(0, 80, 2), 1);
    let delayed = rwdt_due(&machine_with_efuse(efuse));
    assert!(
        delayed > plain,
        "WDT_DELAY_SEL 1 left the hold at {delayed:?}, the same as {plain:?}"
    );
}

/// Arms SYSTIMER alarm 0 in period mode on counter 1, every `period` ticks (16 per us).
fn arm_periodic_systimer(m: &mut Machine, period: u32) {
    const BASE: u32 = 0x6002_3000;
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| {
        bus.store_slow(BASE + 0x014, 4, 0, &view); // UNIT1_LOAD_HI
        bus.store_slow(BASE + 0x018, 4, 0, &view); // UNIT1_LOAD_LO
        bus.store_slow(BASE + 0x060, 4, 1, &view); // UNIT1_LOAD
        // TARGET0_CONF: period, PERIOD_MODE (bit 30), UNIT_SEL counter 1 (bit 31).
        bus.store_slow(BASE + 0x034, 4, period | 1 << 30 | 1 << 31, &view);
        bus.store_slow(BASE + 0x050, 4, 1, &view); // COMP0_LOAD
        bus.store_slow(BASE + 0x064, 4, 1, &view); // INT_ENA
        // CONF: the reset value 0x4600_0000 plus counter 1 (bit 29) and alarm 0 (bit 24).
        bus.store_slow(BASE, 4, 0x4600_0000 | 1 << 29 | 1 << 24, &view);
    });
}

const INTC: u32 = 0x600C_2000;

/// Routes `source` to CPU line `line` at priority 1 over threshold 1, as `esp_intr_alloc` does.
fn route_to_line(m: &mut Machine, source: u8, line: u32) {
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| {
        bus.store_slow(INTC + 4 * u32::from(source), 4, line, &view); // MAP
        bus.store_slow(INTC + 0x114 + 4 * line, 4, 1, &view); // CPU_INT_PRI_n
        bus.store_slow(INTC + 0x194, 4, 1, &view); // CPU_INT_THRESH
        bus.store_slow(INTC + 0x104, 4, 1 << line, &view); // CPU_INT_ENABLE
    });
}

fn systimer_int_ena(m: &mut Machine, val: u32) {
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| bus.store_slow(0x6002_3064, 4, val, &view));
}

/// `wfi; j .`: wait, then spin without touching a register.
const WFI_THEN_SPIN: [u32; 2] = [0x1050_0073, 0x0000_006F];

#[test]
fn a_wfi_hart_whose_periodic_alarm_is_not_routed_is_a_deadlock() {
    let mut m = machine();
    disarm_flash_boot_hold(&mut m);
    detach_usb(&mut m);
    arm_periodic_systimer(&mut m, 16_000);
    load_program(&mut m, &WFI_THEN_SPIN);
    let out = m.run(until(VTime::from_ms(100)));
    assert_eq!(out.reason, StopReason::Deadlock);
    assert!(m.hart.wfi);
    assert_eq!(out.insns, 1, "only the wfi retired");
    assert_eq!(
        out.idle_ps, 0,
        "a deadlock is reported where the hart stopped"
    );
    assert!(
        m.sched.next_time().is_some(),
        "the periodic alarm is still pending: pending events do not make the hart live"
    );
    assert!(!m.interrupt_can_wake(), "a reset alone wakes it");
    // `Deadlock` means wait for input: a later limit gives the same stop at the same instant.
    let again = m.run(until(VTime::from_ms(5_000)));
    assert_eq!(again.reason, StopReason::Deadlock);
    assert_eq!((again.vt, again.insns, again.idle_ps), (out.vt, 0, 0));
}

#[test]
fn a_wfi_hart_whose_periodic_alarm_is_routed_wakes_at_the_first_period() {
    let mut m = machine();
    disarm_flash_boot_hold(&mut m);
    detach_usb(&mut m);
    arm_periodic_systimer(&mut m, 16_000);
    route_to_line(&mut m, pemu_core::irq_source::irq::SYSTIMER_TARGET0.0, 1);
    load_program(&mut m, &WFI_THEN_SPIN);
    assert!(
        m.interrupt_can_wake(),
        "a routed, enabled line above the threshold"
    );
    let out = m.run(until(VTime::from_ms(100)));
    assert_eq!(out.reason, StopReason::Until);
    assert!(!m.hart.wfi, "the pending line woke the hart");
    assert_eq!(
        m.hart.pc,
        PROG + 4,
        "MIE is clear, so it runs on after the wfi"
    );
    // 16,000 SYSTIMER ticks are 1 ms.
    assert_eq!(out.idle_ps, VTime::from_ms(1).0 - m.clock.ps_per_insn());
}

/// Out of reset the rate is XTAL/2 = 20 MHz, so a million instructions are 50 ms, 50 periods of
/// a 1 ms tick.
#[test]
fn an_instruction_limit_bounds_idling_through_a_periodic_event() {
    let mut m = machine();
    disarm_flash_boot_hold(&mut m);
    arm_periodic_systimer(&mut m, 16_000);
    // Routed and enabled, but INT_ENA is off: the hart idles through every period.
    route_to_line(&mut m, pemu_core::irq_source::irq::SYSTIMER_TARGET0.0, 1);
    systimer_int_ena(&mut m, 0);
    assert!(m.sched.next_time().is_some(), "the periodic alarm is armed");
    m.hart.wfi = true;
    let out = m.run(RunLimits::insns(1_000_000));
    assert_eq!(out.reason, StopReason::MaxInsns);
    assert_eq!(out.insns, 0, "nothing executed; the budget was spent idle");
    assert_eq!(out.vt, VTime::from_ms(50));
    assert_eq!(out.idle_ps, VTime::from_ms(50).0);
    assert!(m.sched.next_time().is_some(), "the alarm re-armed itself");
}

/// `RESET_CAUSE_PROCPU` reads 0x01 only because the block was told a reset happened.
#[test]
fn power_on_latches_the_reset_cause_the_rom_banner_prints() {
    let mut m = machine();
    // The six low bits of RTC_CNTL +0x038, which ROM `rtc_get_reset_reason` reads.
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: ROM_BASE,
    };
    let read = m.with_bus(|bus, _| bus.load_slow(0x6000_8038, 4, &view));
    let Access::Ok(state) = read else {
        panic!("RTC_CNTL_RESET_STATE is a plain register read, not a stop or a fault");
    };
    assert_eq!(
        state & 0x3F,
        u32::from(ResetCause::POWERON.0),
        "the machine never told RTC_CNTL a reset happened, so the ROM prints `rst:0x0 (N/A)` \
         and then `invalid reset`"
    );
}

#[test]
fn power_on_reaches_every_block_and_republishes_the_pins_it_dropped() {
    let mut m = machine();
    // Drive GPIO21 (the backlight) high and tell the board, as the GPIO wiring would.
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    m.with_bus(|bus, _| {
        bus.store_slow(0x6000_4020, 4, 1 << 21, &view);
        bus.store_slow(0x6000_4004, 4, 1 << 21, &view);
        let now = bus.inner.cx.now;
        pemu_soc_c3::wiring::gpio::apply(&mut bus.inner.soc.devices.gpio, bus.inner.board, now);
    });
    assert!(m.soc.devices.gpio.out_enabled(21));

    let done = m.power_on();
    assert_eq!(done.after.blocks as usize, pemu_soc_c3::periph::BLOCK_COUNT);
    assert!(done.after.republish_gpio && done.after.rebase_clock);
    assert_eq!(
        done.gpio_pins_published, 1,
        "the reset dropped GPIO21 and the board was not told"
    );
    assert!(!m.soc.devices.gpio.out_enabled(21));
    assert_eq!(
        m.soc.devices.gpio.changed_pins(),
        0,
        "the drop was published"
    );
}

#[test]
fn an_origin_carrying_input_raises_the_receipt_class() {
    use pemu_core::journal::Determinism;
    let mut m = machine();
    let press = InputEvent::Button {
        id: pemu_core::input::ButtonId::Ok,
        down: true,
    };
    m.input(At::Now, press.clone()).expect("journaled");
    assert_eq!(m.receipt().determinism, Some(Determinism::Deterministic));
    let api: &mut dyn MachineApi = &mut m;
    api.input_from(At::Now, Origin::Endpoint, press.clone())
        .expect("journaled");
    assert_eq!(m.receipt().determinism, Some(Determinism::Replayable));
    assert_eq!(m.journal().entries()[1].origin, Origin::Endpoint);
    m.input_from(At::Now, Origin::Bridge, press)
        .expect("journaled");
    assert_eq!(m.receipt().determinism, Some(Determinism::Live));
}

/// Pure execution costs the same cycles at either rate. Boot timestamps still depend on the rate:
/// a poll loop spanning a wait fixed in virtual time retires instructions in proportion to it.
#[test]
fn the_cycle_counter_is_modelled_independent_of_the_reset_rate() {
    let run = |hz: u32| {
        let mut m = machine();
        let insns = m.hart.insns;
        m.clock.rebase(insns, hz);
        m.clock.set_counting(insns, true);
        let out = m.run(RunLimits::insns(50_000));
        let insns = m.hart.insns;
        assert!(out.insns > 0);
        (m.clock.cycle_count(insns), m.now())
    };
    let (cc40, vt40) = run(40_000_000);
    let (cc20, vt20) = run(20_000_000);
    assert_eq!(cc40, cc20, "the cycle count is the same at either rate");
    assert!(
        vt20 > vt40,
        "the slower rate takes longer: {vt20:?} {vt40:?}"
    );
}

/// A reset and a `ClockChanged` rebase through the same function, and the rate follows the
/// board's `[soc] xtal_hz` rather than `system::XTAL_MHZ`.
#[test]
fn a_clock_write_and_a_reset_agree_on_the_reset_rate() {
    const SYSCLK_CONF: u32 = 0x600C_0058;
    const CPU_PER_CONF: u32 = 0x600C_0008;
    let mut m = machine();
    let write = |m: &mut Machine, addr: u32, val: u32| {
        let view = HartView {
            insns: m.hart.insns,
            extra: m.hart.extra,
            pc: PROG,
        };
        m.with_bus(|bus, _| bus.store_slow(addr, 4, val, &view));
        m.apply_pending_wiring();
        m.clock.cpu_hz()
    };
    assert_eq!(m.clock.cpu_hz(), 20_000_000, "power-on");
    assert_eq!(write(&mut m, CPU_PER_CONF, 1), 20_000_000);
    assert_eq!(
        write(&mut m, SYSCLK_CONF, 1 << 10 | 1),
        160_000_000,
        "PLL, CPUPERIOD_SEL 1"
    );
    assert_eq!(
        write(&mut m, SYSCLK_CONF, 1),
        20_000_000,
        "back to the reset value"
    );
    assert_eq!(write(&mut m, SYSCLK_CONF, 3), 10_000_000, "XTAL/4");
    m.chip_reset(ResetKind::of(ResetCause::RTC_SW_SYS).expect("0x03 is a reset cause"));
    assert_eq!(m.clock.cpu_hz(), 20_000_000, "a system reset");
}

/// A power-on reset leaves the record at 0 because the fan-out clears the Chip-scope record
/// afterwards (`specs/blocks/assist_debug.toml`).
#[test]
fn the_reset_record_holds_the_pc_except_after_a_power_on() {
    const RCD_EN: u32 = 0x600C_E044;
    let mut m = machine();
    let view = HartView {
        insns: 0,
        extra: 0,
        pc: PROG,
    };
    let enable = |m: &mut Machine| {
        m.with_bus(|bus, _| bus.store_slow(RCD_EN, 4, 3, &view));
    };
    enable(&mut m);
    m.hart.pc = PROG + 0x10;
    m.hart.x[2] = 0x3FC8_1000;
    m.chip_reset(ResetKind::of(ResetCause::RTC_SW_SYS).expect("0x03"));
    assert_eq!(
        m.soc.devices.assist_debug.reset_record(),
        (PROG + 0x10, 0x3FC8_1000)
    );

    enable(&mut m);
    m.hart.pc = PROG + 0x20;
    m.hart.x[2] = 0x3FC8_2000;
    m.chip_reset(ResetKind::of(ResetCause::POWERON).expect("0x01"));
    assert_eq!(m.soc.devices.assist_debug.reset_record(), (0, 0));
}

#[test]
fn every_reset_stops_and_clears_the_performance_counter() {
    use pemu_rv32::csr::{CSR_MPCCR, CSR_MPCER, CSR_MPCMR, CsrOp, MPCER_CYCLE};
    for cause in [
        ResetCause::POWERON,
        ResetCause::RTC_SW_SYS,
        ResetCause::RTC_SW_CPU,
    ] {
        let mut m = machine();
        let insns = m.hart.insns;
        let csr = |m: &mut Machine, num, op| {
            m.soc
                .counter_csr(&mut m.clock, num, op, insns)
                .expect("modelled")
                .0
        };
        csr(&mut m, CSR_MPCER, CsrOp::Write(MPCER_CYCLE));
        csr(&mut m, CSR_MPCCR, CsrOp::Write(1234));
        assert!(m.clock.counting());
        m.chip_reset(ResetKind::of(cause).expect("a documented cause"));
        assert!(!m.clock.counting(), "{cause:?}: the counter is stopped");
        assert_eq!(csr(&mut m, CSR_MPCCR, CsrOp::Read), 0, "{cause:?}");
        assert_eq!(csr(&mut m, CSR_MPCER, CsrOp::Read), 0, "{cause:?}");
        assert_eq!(
            csr(&mut m, CSR_MPCMR, CsrOp::Read),
            Csr::new().mpcmr,
            "{cause:?}"
        );
    }
}

#[test]
fn the_reset_clock_is_half_the_board_crystal() {
    let m = machine();
    // Half of `[soc] xtal_hz`, not the 160 MHz the bootloader raises SYSCLK to.
    assert_eq!(m.clock.cpu_hz(), m.cfg.board.xtal_hz / 2);
    assert_eq!(m.cfg.board.xtal_hz, 40_000_000);
    assert_eq!(m.clock.cpu_hz(), 20_000_000);
}

#[test]
fn receipt_drains_the_ledger_delta_and_moves_its_cursor() {
    let mut m = machine();
    m.run(RunLimits::insns(200_000));
    let (touches, cursor) = m.first_touches_since(0);
    assert!(
        !touches.is_empty(),
        "the ROM touches peripherals before it prints"
    );
    assert_eq!(m.receipt_cursor(), 0, "nothing has been drained yet");
    m.receipt();
    assert_eq!(m.receipt_cursor(), cursor);
    assert!(m.first_touches_since(m.receipt_cursor()).0.is_empty());
}

#[test]
fn the_receipt_fidelity_lists_are_the_whole_run_and_not_the_drained_delta() {
    let mut m = machine();
    m.run(RunLimits::insns(200_000));
    let first = m.receipt();
    assert!(
        !first.classes_touched.c.is_empty(),
        "the ROM leans on class C blocks before it prints"
    );
    let second = m.receipt();
    assert_eq!(first.classes_touched, second.classes_touched);
    assert_eq!(first.unmodeled_first_touch, second.unmodeled_first_touch);
    assert!(m.first_touches_since(m.receipt_cursor()).0.is_empty());
}

#[test]
fn a_modeled_register_is_not_a_class_u_caveat() {
    let mut m = machine();
    m.run(RunLimits::insns(200_000));
    let receipt = m.receipt();
    assert!(
        !receipt.classes_touched.c.is_empty(),
        "the ROM run leans on class C blocks"
    );
    // The ledger notes nothing for `system`: the class has to come from the model.
    let system = pemu_soc_c3::periph::id::SYSTEM;
    let touched = m
        .ledger()
        .first_touches()
        .iter()
        .find(|t| t.periph == system)
        .copied()
        .expect("the ROM touches SYSTEM");
    assert!(
        m.ledger()
            .notes()
            .iter()
            .all(|n| n.subject != touched.subject()),
        "the run recorded no note for it, so step 2 is what answered"
    );
    assert_ne!(
        m.soc.devices.fidelity_of(system, touched.off),
        Some(pemu_core::fidelity::Fidelity::U),
        "the model claims a class for it"
    );
    assert!(
        !receipt
            .unmodeled_first_touch
            .iter()
            .any(|name| name.starts_with("system.")),
        "a modeled SYSTEM register is no unmodeled first touch: {:?}",
        receipt.unmodeled_first_touch
    );
}

/// TWAI has no generated register table, which exercises the `block.0xNNNN` fallback.
#[test]
fn a_store_into_an_unmodeled_block_is_a_class_u_caveat_naming_the_register() {
    const TWAI_MODE: u32 = 0x6002_B000;
    let mut m = machine();
    let view = HartView {
        insns: m.hart.insns,
        extra: m.hart.extra,
        pc: PROG,
    };
    m.with_bus(|bus, _| bus.store_slow(TWAI_MODE, 4, 1, &view));
    let receipt = m.receipt();
    assert!(
        receipt.classes_touched.u.iter().any(|s| s == "twai"),
        "{:?}",
        receipt.classes_touched
    );
    assert!(
        receipt
            .unmodeled_first_touch
            .iter()
            .any(|s| s == "twai.0x0000"),
        "a block with no generated table names the offset: {:?}",
        receipt.unmodeled_first_touch
    );
}

#[test]
fn the_receipt_reports_the_machines_cpi_and_journal_length() {
    let mut m = machine();
    assert_eq!(m.receipt().cpi_milli, Some(m.clock.cpi_milli()));
    assert_eq!(m.receipt().journal_len, Some(0));
    m.input(
        At::Now,
        InputEvent::SerialIn {
            chan: SerialChan::USJ,
            data: b"x".to_vec(),
        },
    )
    .expect("the console takes input");
    assert_eq!(
        m.receipt().journal_len,
        Some(1),
        "the journal is part of run identity"
    );
}
