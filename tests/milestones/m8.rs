//! Milestone M8 tests: BLE HLE, the virtual LE controller and air, and the external HCI bridge.
//! Names use the prefix `t<tier>_m8_` so `xtask ci` can count them.

// Shared helpers; not every milestone uses every helper.
#[allow(dead_code)]
mod common;
use common::{machine, one_bench_record, pk_files, workspace, xtask_bench};
// The fresh-process leg of the determinism harness, shared with m1.rs, m3.rs and m12.rs.
#[allow(dead_code)]
mod determinism;

use std::sync::Arc;

use pemu_core::hostio::SerialStream;
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_loader::elf::ElfInfo;
use pemu_loader::esp_image::MergedImage;
use pemu_machine::Executor;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::hle::{HleFeatureStatus as FeatureStatus, HleTripKind as TripKind};
use pemu_machine::machine::Machine;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{StopReason, StopSet};

fn one_second() -> RunLimits {
    RunLimits {
        until: Some(VTime::from_ms(1_000)),
        max_insns: None,
        stops: StopSet::default(),
    }
}

fn console(m: &mut Machine) -> String {
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let bytes: Vec<u8> = ring.slices(ring.tail()).iter().copied().collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn ble_disabled() -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.hle.disabled = vec!["ble".to_string()];
    cfg
}

/// The 7 VHCI entries that hold a hook other than a tripwire: the BLE module's handler hooks. A
/// disabled or refused module leaves `esp_bt_controller_init` a tripwire and the others unhooked.
const VHCI: [&str; 7] = [
    "esp_bt_controller_init",
    "esp_bt_controller_deinit",
    "esp_bt_controller_enable",
    "esp_bt_controller_disable",
    "esp_vhci_host_check_send_available",
    "esp_vhci_host_send_packet",
    "esp_vhci_host_register_callback",
];

fn ble_hooks(m: &Machine) -> usize {
    let elf = m.assets().app_elf.clone().expect("elf");
    VHCI.iter()
        .filter_map(|name| elf.symbols.addr_of(name))
        .filter(|pc| m.hle_binding().set.get(*pc).is_some() && m.tripwires().at(*pc).is_none())
        .count()
}

/// Applies `edit` to `len` bytes at `offset` into `symbol`, in a copy of the merged flash image
/// and of the ELF file, keeping the app image checksum and its appended SHA-256 valid.
fn patch_symbol(
    flash: &mut [u8],
    elf_file: &mut [u8],
    symbol: &str,
    offset: u32,
    len: usize,
    edit: impl Fn(&mut [u8]),
) {
    let elf = ElfInfo::parse(elf_file).expect("the ELF parses");
    let addr = elf.symbols.addr_of(symbol).expect("the symbol is linked") + offset;
    let section = elf
        .sections
        .iter()
        .find(|s| s.is_alloc() && s.has_bits() && addr >= s.addr && u64::from(addr) < s.end())
        .expect("a section holds the bytes");
    let at = (section.offset + (addr - section.addr)) as usize;
    edit(&mut elf_file[at..at + len]);
    let merged = MergedImage::parse(flash).expect("the image parses");
    let (_, app) = merged.app.expect("pk has a boot app");
    let seg = app
        .segments
        .iter()
        .find(|s| addr >= s.load_addr && addr + len as u32 <= s.load_addr + s.len)
        .expect("a segment loads the bytes");
    let at = seg.data_offset + (addr - seg.load_addr) as usize;
    let old = flash[at..at + len].to_vec();
    edit(&mut flash[at..at + len]);
    let xor = old
        .iter()
        .zip(&flash[at..at + len])
        .fold(0u8, |x, (o, n)| x ^ o ^ n);
    let end = app.offset + app.len;
    assert!(app.header.hash_appended, "pk appends the image hash");
    flash[end - 33] ^= xor;
    let digest = pemu_loader::sha256(&flash[app.offset..end - 32]);
    flash[end - 32..end].copy_from_slice(&digest);
    let (_, again) = MergedImage::parse(flash)
        .expect("the altered image parses")
        .app
        .expect("app");
    assert!(
        again.checksum_ok(),
        "the altered image keeps a valid checksum"
    );
    assert_eq!(again.hash_ok(), Some(true), "and a valid appended hash");
}

fn alter_first_32_bytes(flash: &mut [u8], elf_file: &mut [u8], symbol: &str) {
    patch_symbol(flash, elf_file, symbol, 0, 32, |bytes| {
        for b in bytes {
            *b ^= 0xFF;
        }
    });
}

/// The BLE status and mismatches of `pk` after `patch`.
fn ble_after(
    flash: &[u8],
    elf: &[u8],
    patch: impl Fn(&mut [u8], &mut [u8]),
) -> (Option<FeatureStatus>, String) {
    let (mut flash, mut elf) = (flash.to_vec(), elf.to_vec());
    patch(&mut flash, &mut elf);
    let m = machine(&flash, &elf, MachineConfig::default());
    (
        m.hle_binding().record.features.get("ble").cloned(),
        format!("{:?}", m.hle_binding().mismatches),
    )
}

/// The binding hashes the relocation-masked skeleton, so a `pk` whose `gp`-relative immediates
/// moved (a relink) still binds, while a different opcode and a different `idf_ver` refuse.
#[test]
fn t1_m8_skeleton_binding_survives_a_relink_and_nothing_else() {
    let test = "t1_m8_skeleton_binding_survives_a_relink_and_nothing_else";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    const INIT: &str = "esp_bt_controller_init";
    // pk's `esp_bt_controller_init` at +10, after 5 compressed instructions: `lw a5, imm(gp)`
    // then `bnez a5, imm`.
    let word = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let (status, why) = ble_after(&flash, &elf, |f, e| {
        patch_symbol(f, e, INIT, 10, 8, |b| {
            let (lw, bne) = (word(&b[0..4]), word(&b[4..8]));
            assert_eq!(
                (lw & 0x7F, (lw >> 15) & 0x1F),
                (0x03, 3),
                "a gp-relative load"
            );
            assert_eq!(bne & 0x7F, 0x63, "a branch");
            let lw = (lw & 0x000F_FFFF) | ((lw >> 20).wrapping_add(8) & 0xFFF) << 20;
            let bne = (bne & 0x01FF_F07F) | 0x0000_0400; // another branch offset
            b[0..4].copy_from_slice(&lw.to_le_bytes());
            b[4..8].copy_from_slice(&bne.to_le_bytes());
        })
    });
    assert_eq!(
        status,
        Some(FeatureStatus::Bound),
        "shifted immediates bind: {why}"
    );

    let (status, why) = ble_after(&flash, &elf, |f, e| {
        patch_symbol(f, e, INIT, 10, 1, |b| b[0] = (b[0] & !0x7F) | 0x33)
    });
    assert_eq!(
        status,
        Some(FeatureStatus::UnsupportedImage),
        "wrong opcode"
    );
    assert!(why.contains("CodeHash"), "{why}");

    let (status, why) = ble_after(&flash, &elf, |f, e| {
        patch_symbol(f, e, "esp_app_desc", 112, 32, |b| {
            b.fill(0);
            b[..6].copy_from_slice(b"v5.5.2");
        })
    });
    assert_eq!(
        status,
        Some(FeatureStatus::UnsupportedImage),
        "wrong idf_ver"
    );
    assert!(
        why.contains("IdfVersion") && why.contains("v5.5.2"),
        "{why}"
    );
}

/// Fail-closed binding: `pk` with the first 32 bytes of `esp_vhci_host_send_packet` altered marks
/// BLE `unsupported image` in the receipt, binds no BLE hook, and stops at the tripwire. The
/// unaltered pair binds all 7 hooks.
#[test]
fn t1_m8_fail_closed_binding() {
    let test = "t1_m8_fail_closed_binding";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let control = machine(&flash, &elf, MachineConfig::default());
    assert_eq!(
        control.hle_binding().record.features.get("ble"),
        Some(&FeatureStatus::Bound),
        "{id}: {:?}",
        control.hle_binding().mismatches
    );
    assert_eq!(ble_hooks(&control), 7, "{id}: the 7 VHCI hooks bind on pk");

    let (mut flash2, mut elf2) = (flash.clone(), elf.clone());
    alter_first_32_bytes(&mut flash2, &mut elf2, "esp_vhci_host_send_packet");
    for executor in [Executor::Engine, Executor::Reference] {
        let mut m = machine(&flash2, &elf2, MachineConfig::default());
        m.set_executor(executor);
        let record = m
            .receipt()
            .binding
            .expect("the receipt carries the binding");
        assert_eq!(
            record.features.get("ble").map(|s| s.receipt_word()),
            Some("unsupported image"),
            "{id}"
        );
        let mismatches = &m.hle_binding().mismatches;
        assert_eq!(mismatches.len(), 1, "{id}: {mismatches:?}");
        assert_eq!(mismatches[0].0, "ble");
        assert!(
            mismatches[0]
                .1
                .iter()
                .any(|mm| mm.symbol == "esp_vhci_host_send_packet"
                    && format!("{:?}", mm.field) == "CodeHash"),
            "{id}: {mismatches:?}"
        );
        assert_eq!(ble_hooks(&m), 0, "{id}: no BLE hook is bound");

        let entry = m
            .assets()
            .app_elf
            .as_ref()
            .and_then(|e| e.symbols.addr_of("esp_bt_controller_init"))
            .expect("pk links the BLE init");
        let out = m.run(one_second());
        let text = console(&mut m);
        let StopReason::Tripwire(report) = &out.reason else {
            panic!("{id} {executor:?}: pk ended {:?}", out.reason);
        };
        assert_eq!(report.kind, TripKind::DisabledFeature, "{id}");
        assert_eq!((report.pc, report.feature), (entry, Some("ble")), "{id}");
        assert!(text.contains("bsp_lvgl: LVGL 就绪"), "{id}");
        assert!(!text.contains("BLE_INIT"), "{id}");
    }
}

/// The HLE A/B prefix test passes on `pk`, and `esp_coex_adapter_register` and `coex_pre_init`
/// return with no tripwire.
///
/// Run B has BLE disabled and stops at the `esp_bt_controller_init` tripwire; run A binds it, so
/// B's console is a prefix of A's, on both executors. Both coexistence functions stay real: an
/// observe hook at each return address counts the return, and run A reaches 1 s with no tripwire.
#[test]
fn t1_m8_hle_ab_prefix_and_coex_runs_real() {
    let test = "t1_m8_hle_ab_prefix_and_coex_runs_real";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for executor in [Executor::Engine, Executor::Reference] {
        let mut b = machine(&flash, &elf, ble_disabled());
        b.set_executor(executor);
        let out_b = b.run(one_second());
        assert!(
            matches!(&out_b.reason, StopReason::Tripwire(r) if r.kind == TripKind::DisabledFeature),
            "{id} {executor:?}: B ended {:?}",
            out_b.reason
        );
        let text_b = console(&mut b);

        let mut a = machine(&flash, &elf, MachineConfig::default());
        a.set_executor(executor);
        let symbols = a.assets().app_elf.clone().expect("elf").symbols.clone();
        let entries: Vec<u32> = ["esp_coex_adapter_register", "coex_pre_init"]
            .iter()
            .map(|name| symbols.addr_of(name).expect("pk links coexistence"))
            .collect();
        let mut returns = Vec::new();
        let out_a = loop {
            let out = a.run(RunLimits {
                until: Some(VTime::from_ms(1_000)),
                max_insns: None,
                stops: StopSet {
                    breakpoints: entries.clone(),
                    ..StopSet::default()
                },
            });
            match out.reason {
                StopReason::Breakpoint(pc) if entries.contains(&pc) => {
                    let ra = a.hart().x[1];
                    a.add_observe_hook(ra, "coex return");
                    returns.push((pc, ra));
                }
                _ => break out,
            }
        };
        assert_eq!(out_a.reason, StopReason::Until, "{id} {executor:?}");
        let text_a = console(&mut a);
        assert!(
            text_a.starts_with(&text_b),
            "{id} {executor:?}: B's console is not a prefix of A's"
        );
        assert!(text_a.len() > text_b.len());
        assert_eq!(
            text_a[text_b.len()..]
                .lines()
                .filter(|l| l.contains("BLE_INIT:"))
                .count(),
            4,
            "{id}"
        );
        // The receipt names the profile and counts the 5 lines whose `esp_log` returned.
        let record = a
            .receipt()
            .binding
            .expect("the receipt carries the binding");
        assert_eq!(record.profile_id, "idf-5.5.3", "{id}");
        assert_eq!(
            record
                .log_lines
                .get("ble")
                .map(|l| (l.synthesized, l.verified)),
            Some((5, true)),
            "{id} {executor:?}: pk's shape prints its sdkconfig lines, verified"
        );
        assert!(text_a.contains("phy_init: phy_version"), "{id}");
        for entry in &entries {
            let (_, ra) = returns
                .iter()
                .find(|(pc, _)| pc == entry)
                .unwrap_or_else(|| panic!("{id}: {entry:#010x} never ran"));
            assert!(
                a.observe_fires(*ra).is_some_and(|(n, _)| n >= 1),
                "{id} {executor:?}: the call at {entry:#010x} never returned"
            );
        }
    }
}

/// Binding BLE does not change the first screen: a BLE-bound `pk` run to the instant the
/// BLE-disabled `pk` stops at holds the same panel frame, on both executors.
#[test]
fn t1_m8_the_first_screen_is_unchanged_with_ble_bound() {
    use pemu_board::st7789::FrameView;
    let test = "t1_m8_the_first_screen_is_unchanged_with_ble_bound";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for executor in [Executor::Engine, Executor::Reference] {
        let mut disabled = machine(&flash, &elf, ble_disabled());
        disabled.set_executor(executor);
        let out = disabled.run(one_second());
        assert!(
            matches!(&out.reason, StopReason::Tripwire(r) if r.kind == TripKind::DisabledFeature),
            "{test} {executor:?}: {:?}",
            out.reason
        );
        let instant = disabled.now();
        let first_screen = disabled.board().lcd.frame(FrameView::Raw);

        let mut bound = machine(&flash, &elf, MachineConfig::default());
        bound.set_executor(executor);
        let out = bound.run(RunLimits {
            until: Some(instant),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test} {executor:?}");
        assert_eq!(bound.now(), instant, "{test} {executor:?}");
        assert!(
            bound.board().lcd.frame(FrameView::Raw) == first_screen,
            "{test} {executor:?}: the first screen differs with BLE bound"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The U4 gate through a real `Machine`
// ---------------------------------------------------------------------------------------------

use pemu_core::input::InputEvent;
use pemu_core::irq_source::IrqSource;
use pemu_core::journal::{LiveStream, Origin};
use pemu_core::snap::{SnapOpts, Snapshot};
use pemu_machine::hle::{HleMagicKind, HleWakeMode};
use pemu_machine::machine::At;

/// HCI Command Complete for HCI Reset (0x0C03), status 0: the answer NimBLE is waiting for after
/// `pk`'s BLE init, so delivering it is harmless and visible.
const RESET_COMPLETE: [u8; 7] = [0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00];

struct GateSyms {
    yield_from_isr: u32,
    host_rcv_pkt: u32,
    enter_critical: u32,
    exit_critical: u32,
    current_tcb: u32,
}

/// `pk` in U4 booted to 1 s: the worker exists, the radio interrupt has the magic ISR, and the
/// worker waits on its semaphore.
fn u4_pk(test: &str, executor: Executor) -> Option<(Machine, GateSyms, Vec<u8>, Vec<u8>)> {
    let (flash, elf) = pk_files(test)?;
    let mut m = u4_machine(&flash, &elf, executor);
    let out = m.run(one_second());
    assert_eq!(out.reason, StopReason::Until, "{test} {executor:?}");
    let symbols = m.assets().app_elf.clone().expect("elf").symbols.clone();
    let at = |name: &str| {
        symbols
            .addr_of(name)
            .unwrap_or_else(|| panic!("pk links {name}"))
    };
    let syms = GateSyms {
        yield_from_isr: at("vPortYieldFromISR"),
        host_rcv_pkt: at("host_rcv_pkt"),
        enter_critical: at("vPortEnterCritical"),
        exit_critical: at("vPortExitCritical"),
        current_tcb: at("pxCurrentTCBs"),
    };
    let worker = m
        .radio_worker(HleMagicKind::BtWorker)
        .expect("the worker started");
    assert_ne!(
        worker.semaphore, 0,
        "{test}: the worker waits on its semaphore"
    );
    assert!(!worker.wake.level());
    Some((m, syms, flash, elf))
}

/// How many times the radio source has been lowered. NimBLE's startup commands go through the
/// same worker, so a gate test counts from its own start.
fn lowered(m: &Machine) -> u32 {
    m.radio_worker(HleMagicKind::BtWorker)
        .map(|w| w.wake.lowered())
        .unwrap_or(0)
}

fn u4_machine(flash: &[u8], elf: &[u8], executor: Executor) -> Machine {
    let mut cfg = MachineConfig::default();
    cfg.hle.wake = HleWakeMode::U4MagicIsr;
    let mut m = machine(flash, elf, cfg);
    m.set_executor(executor);
    m
}

/// Journals one packet from an external controller with `Origin::Bridge`, so a gate run reports
/// `live` and its replay `deterministic`. It applies at [`apply`].
fn journal_event(m: &mut Machine, seq: u64) {
    m.input_from(
        At::Now,
        Origin::Bridge,
        InputEvent::HciPacket {
            seq,
            data: RESET_COMPLETE.to_vec(),
        },
    )
    .expect("an input at the current instant is accepted");
}

/// Applies every journaled input due now: the run loop applies the journal before it checks its
/// limits, so a zero-instruction run is exactly that step.
fn apply(m: &mut Machine) {
    let out = m.run(RunLimits::insns(0));
    assert_eq!(out.insns, 0, "a zero-instruction run runs nothing");
}

/// The interrupt source of the BLE module's magic ISR (`ble.toml` `[worker] isr_source`).
fn ble_source() -> IrqSource {
    IrqSource(
        pemu_radio::ble::profile::BleProfile::load()
            .worker
            .isr_source,
    )
}

fn post(m: &mut Machine) -> IrqSource {
    let seq = m.journal().live_next(LiveStream::Hci);
    journal_event(m, seq);
    apply(m);
    let source = ble_source();
    assert!(
        m.irq_source_level(source),
        "the applied input raised the radio source"
    );
    source
}

fn until(ms: u64, breakpoints: Vec<u32>) -> RunLimits {
    RunLimits {
        until: Some(VTime::from_ms(ms)),
        max_insns: None,
        stops: StopSet {
            breakpoints,
            ..StopSet::default()
        },
    }
}

fn current_task(m: &mut Machine, syms: &GateSyms) -> u32 {
    m.guest_mem()
        .load(syms.current_tcb, 4)
        .expect("pxCurrentTCBs is RAM")
}

fn in_magic_isr(m: &Machine) -> bool {
    m.hle_section()
        .continuations
        .iter()
        .any(|(_, c)| c.handler.handler == "core.magic_isr")
}

/// Runs to where the magic ISR's nested `xQueueGiveFromISR` has returned with the worker woken
/// and its yield-from-ISR call is starting. Other interrupts' yields are run past.
fn to_magic_isr_yield(m: &mut Machine, syms: &GateSyms, test: &str) {
    loop {
        let out = m.run(until(1_500, vec![syms.yield_from_isr]));
        match out.reason {
            StopReason::Breakpoint(pc) if pc == syms.yield_from_isr && in_magic_isr(m) => return,
            StopReason::Breakpoint(_) => continue,
            other => panic!("{test}: no magic ISR yield, run ended {other:?}"),
        }
    }
}

fn to_delivery(m: &mut Machine, syms: &GateSyms, test: &str) {
    let out = m.run(until(1_500, vec![syms.host_rcv_pkt]));
    assert_eq!(
        out.reason,
        StopReason::Breakpoint(syms.host_rcv_pkt),
        "{test}: the event never reached notify_host_recv"
    );
    let (a0, a1) = (m.hart().x[10], m.hart().x[11]);
    assert_eq!(a1, RESET_COMPLETE.len() as u32, "{test}");
    let got: Vec<u8> = (0..a1)
        .map(|i| m.guest_mem().load(a0 + i, 1).expect("RAM") as u8)
        .collect();
    assert_eq!(
        got, RESET_COMPLETE,
        "{test}: the packet on the worker stack"
    );
}

const BOTH: [Executor; 2] = [Executor::Engine, Executor::Reference];

/// U4 gate point 1: the worker wakes on a raised radio event, in its own task, and hands the
/// event to the registered VHCI callback.
#[test]
fn t1_m8_u4_gate_1_the_worker_wakes_on_a_raised_event() {
    let test = "t1_m8_u4_gate_1_the_worker_wakes_on_a_raised_event";
    for executor in BOTH {
        let Some((mut m, syms, _, _)) = u4_pk(test, executor) else {
            return;
        };
        let source = post(&mut m);
        assert!(
            m.irq_source_level(source),
            "{test}: the post raised the source"
        );
        to_delivery(&mut m, &syms, test);
        let worker = m
            .radio_worker(HleMagicKind::BtWorker)
            .expect("worker")
            .clone();
        assert_eq!(
            current_task(&mut m, &syms),
            worker.task,
            "{test} {executor:?}: in the worker"
        );
        let out = m.run(one_second_more(&m));
        assert_eq!(out.reason, StopReason::Until, "{test} {executor:?}");
        assert_eq!(
            m.radio_worker(HleMagicKind::BtWorker)
                .unwrap()
                .wake
                .queued(),
            0
        );
    }
}

fn one_second_more(m: &Machine) -> RunLimits {
    RunLimits {
        until: Some(VTime::from_us(m.now().as_us() + 100_000)),
        max_insns: None,
        stops: StopSet::default(),
    }
}

/// U4 gate point 2: the core lowers the radio source exactly once per event, at the magic ISR
/// return. At the yield call the level is still up; when the worker runs it is down.
#[test]
fn t1_m8_u4_gate_2_the_level_is_lowered_once_per_event_at_the_magic_isr_return() {
    let test = "t1_m8_u4_gate_2_the_level_is_lowered_once_per_event_at_the_magic_isr_return";
    for executor in BOTH {
        let Some((mut m, syms, _, _)) = u4_pk(test, executor) else {
            return;
        };
        let base = lowered(&m);
        for k in base..base + 3 {
            let source = post(&mut m);
            to_magic_isr_yield(&mut m, &syms, test);
            let wake = &m.radio_worker(HleMagicKind::BtWorker).unwrap().wake;
            assert_eq!(
                wake.lowered(),
                k,
                "{test} {executor:?}: lowered before the return"
            );
            assert!(
                m.irq_source_level(source),
                "{test}: still raised inside the ISR"
            );
            to_delivery(&mut m, &syms, test);
            let wake = &m.radio_worker(HleMagicKind::BtWorker).unwrap().wake;
            assert_eq!(wake.lowered(), k + 1, "{test} {executor:?}: once per event");
            assert!(!m.irq_source_level(source), "{test}: lowered at the return");
            assert!(!wake.level());
            // Let the worker go back to its semaphore, so the next give wakes it.
            let out = m.run(one_second_more(&m));
            assert_eq!(out.reason, StopReason::Until, "{test} {executor:?}");
        }
    }
}

/// U4 gate point 2 with two events posted at once: the second magic ISR finds the worker woken
/// and gives without a yield; the level is lowered twice and one wake delivers both packets in
/// order.
#[test]
fn t1_m8_u4_two_events_raised_together_wake_the_worker_once_and_both_arrive() {
    let test = "t1_m8_u4_two_events_raised_together_wake_the_worker_once_and_both_arrive";
    for executor in BOTH {
        let Some((mut m, syms, _, _)) = u4_pk(test, executor) else {
            return;
        };
        let base = lowered(&m);
        let source = post(&mut m);
        post(&mut m);
        let worker = m.radio_worker(HleMagicKind::BtWorker).unwrap().task;
        to_magic_isr_yield(&mut m, &syms, test);
        let wake = &m.radio_worker(HleMagicKind::BtWorker).unwrap().wake;
        assert_eq!(
            (wake.queued(), wake.lowered()),
            (2, base),
            "{test} {executor:?}"
        );
        to_delivery(&mut m, &syms, test);
        let wake = &m.radio_worker(HleMagicKind::BtWorker).unwrap().wake;
        assert_eq!(
            wake.lowered(),
            base + 2,
            "{test} {executor:?}: once per event"
        );
        assert!(!wake.level() && !m.irq_source_level(source), "{test}");
        assert_eq!(current_task(&mut m, &syms), worker, "{test} {executor:?}");
        let out = m.run(until(1_500, vec![syms.yield_from_isr, syms.host_rcv_pkt]));
        assert_eq!(
            out.reason,
            StopReason::Breakpoint(syms.host_rcv_pkt),
            "{test} {executor:?}: the second event was not delivered by the same wake"
        );
        assert!(!in_magic_isr(&m), "{test}");
        assert_eq!(current_task(&mut m, &syms), worker, "{test} {executor:?}");
        assert_eq!(m.hart().x[11], RESET_COMPLETE.len() as u32, "{test}");
        assert_eq!(
            m.radio_worker(HleMagicKind::BtWorker)
                .unwrap()
                .wake
                .lowered(),
            base + 2,
            "{test} {executor:?}: no third lowering"
        );
    }
}

/// U4 gate point 3: the interrupt exit switches to the priority-23 worker, which runs its upcall
/// within the same tick while the interrupted task was another one.
#[test]
fn t1_m8_u4_gate_3_the_woken_worker_runs_at_interrupt_exit() {
    let test = "t1_m8_u4_gate_3_the_woken_worker_runs_at_interrupt_exit";
    for executor in BOTH {
        let Some((mut m, syms, _, _)) = u4_pk(test, executor) else {
            return;
        };
        post(&mut m);
        to_magic_isr_yield(&mut m, &syms, test);
        let worker = m.radio_worker(HleMagicKind::BtWorker).unwrap().task;
        let interrupted = current_task(&mut m, &syms);
        assert_ne!(interrupted, worker, "{test}: the worker was waiting");
        assert!(
            m.radio_worker(HleMagicKind::BtWorker)
                .unwrap()
                .wake
                .yield_requested()
        );
        let at = m.now();
        to_delivery(&mut m, &syms, test);
        assert_eq!(current_task(&mut m, &syms), worker, "{test} {executor:?}");
        let waited = m.now().as_us() - at.as_us();
        // One FreeRTOS tick is 1 ms (`configTICK_RATE_HZ` 1000).
        assert!(
            waited < 1_000,
            "{test} {executor:?}: the switch waited {waited} us"
        );
    }
}

/// U4 gate point 4: a snapshot taken between the nested `xQueueGiveFromISR` return and the magic
/// ISR return restores in a fresh machine and continues to identical state and output.
#[test]
fn t1_m8_u4_gate_4_a_snapshot_inside_the_magic_isr_restores_identically() {
    let test = "t1_m8_u4_gate_4_a_snapshot_inside_the_magic_isr_restores_identically";
    for executor in BOTH {
        let Some((mut m, syms, flash, elf)) = u4_pk(test, executor) else {
            return;
        };
        let base = lowered(&m);
        let delivered = ble_state(&m).rx_packets;
        post(&mut m);
        to_magic_isr_yield(&mut m, &syms, test);
        let isr = m
            .hle_section()
            .continuations
            .iter()
            .find(|(_, c)| c.handler.handler == "core.magic_isr")
            .map(|(_, c)| c.func)
            .expect("the magic ISR is outstanding");
        assert_eq!(isr, syms.yield_from_isr, "{test}: the give has returned");
        let cursor = m.io().serial_ring(SerialStream::UsjTx).head();
        let bytes = m
            .snapshot(SnapOpts::default())
            .to_bytes()
            .expect("a machine snapshot serializes");
        let mut restored = u4_machine(&flash, &elf, executor);
        restored
            .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
            .expect("restores");
        assert_eq!(
            restored.state_hash(),
            m.state_hash(),
            "{test}: at the instant"
        );
        assert_eq!(
            restored.hle_section(),
            m.hle_section(),
            "{test}: the decoded hle section, outstanding magic ISR included"
        );
        assert_eq!(
            restored.radio_module_state("ble"),
            m.radio_module_state("ble")
        );
        let end = || RunLimits {
            // Past the 2 s NimBLE would wait for an unanswered command.
            until: Some(VTime::from_ms(3_500)),
            max_insns: None,
            stops: StopSet::default(),
        };
        let (a, b) = (m.run(end()), restored.run(end()));
        assert_eq!(
            (a.reason, a.insns),
            (b.reason, b.insns),
            "{test} {executor:?}"
        );
        assert_eq!(restored.state_hash(), m.state_hash(), "{test} {executor:?}");
        let after = |m: &mut Machine| -> Vec<u8> {
            let ring = m.io().serial_ring(SerialStream::UsjTx);
            ring.slices(cursor).iter().copied().collect()
        };
        assert_eq!(
            after(&mut restored),
            after(&mut m),
            "{test} {executor:?}: the output after the instant"
        );
        // Not vacuous: no host reset after the instant, and each machine delivered the event.
        for machine in [&m, &restored] {
            assert_eq!(
                lowered(machine),
                base + 1,
                "{test}: the event finished in both"
            );
            assert_eq!(
                ble_state(machine).rx_packets,
                delivered + 1,
                "{test}: delivered to notify_host_recv in both"
            );
        }
    }
}

/// U4 gate point 5: an event raised inside a critical section is delivered after it ends.
#[test]
fn t1_m8_u4_gate_5_an_event_inside_a_critical_section_is_delivered_after_it() {
    let test = "t1_m8_u4_gate_5_an_event_inside_a_critical_section_is_delivered_after_it";
    for executor in BOTH {
        let Some((mut m, syms, _, _)) = u4_pk(test, executor) else {
            return;
        };
        let out = m.run(until(1_500, vec![syms.enter_critical]));
        assert_eq!(
            out.reason,
            StopReason::Breakpoint(syms.enter_critical),
            "{test}"
        );
        let ra = m.hart().x[1];
        let out = m.run(until(1_500, vec![ra]));
        assert_eq!(
            out.reason,
            StopReason::Breakpoint(ra),
            "{test}: inside the section"
        );
        let generation = m.hle_section().isr_generation;
        let base = lowered(&m);
        let source = post(&mut m);
        let out = m.run(until(1_500, vec![syms.exit_critical]));
        assert_eq!(
            out.reason,
            StopReason::Breakpoint(syms.exit_critical),
            "{test}"
        );
        assert_eq!(
            m.hle_section().isr_generation,
            generation,
            "{test} {executor:?}: no magic ISR ran inside the section"
        );
        assert!(
            m.irq_source_level(source),
            "{test}: the event is still pending"
        );
        assert_eq!(
            m.radio_worker(HleMagicKind::BtWorker)
                .unwrap()
                .wake
                .lowered(),
            base
        );
        to_delivery(&mut m, &syms, test);
        assert!(m.hle_section().isr_generation > generation, "{test}");
        assert_eq!(
            m.radio_worker(HleMagicKind::BtWorker)
                .unwrap()
                .wake
                .lowered(),
            base + 1,
            "{test} {executor:?}: delivered, not lost"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// The virtual LE controller
// ---------------------------------------------------------------------------------------------

use pemu_radio::ble::air::{self, AdvertisingPdu};
use pemu_radio::ble::vhci::BleState;

/// The `FoloPassport` AdvData of the official BLE demo: Flags 0x06 and the Complete Local Name.
const FOLOPASSPORT_ADV: [u8; 17] = [
    0x02, 0x01, 0x06, 0x0D, 0x09, 0x46, 0x6F, 0x6C, 0x6F, 0x50, 0x61, 0x73, 0x73, 0x70, 0x6F, 0x72,
    0x74,
];

fn ble_state(m: &Machine) -> BleState {
    BleState::decode(m.radio_module_state("ble").unwrap_or(&[])).expect("the module state decodes")
}

fn on_air(m: &Machine) -> Option<AdvertisingPdu> {
    let st = ble_state(m);
    let mut public = st.bt_mac;
    public.reverse();
    air::advertising_pdu(&st.controller, public)
}

fn run_to_line(m: &mut Machine, line: &str, limit_ms: u64, test: &str) {
    while !console(m).contains(line) {
        assert!(
            m.now() < VTime::from_ms(limit_ms),
            "{test}: no `{line}` by {limit_ms} ms; console tail:\n{}",
            tail(m)
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 10_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test}: {}", tail(m));
    }
}

fn tail(m: &mut Machine) -> String {
    let text = console(m);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(12)..].join("\n")
}

/// Every acknowledgement carried status 0 and no command was unknown to the controller.
fn assert_all_answered_ok(m: &Machine, test: &str) {
    let c = ble_state(m).controller;
    assert!(c.commands > 0, "{test}: the host sent commands");
    assert_eq!(c.unknown_commands, 0, "{test}: {:02x?}", c.recent);
    assert!(
        c.recent.iter().all(|a| a[2] == 0),
        "{test}: a command failed: {:02x?}",
        c.recent
    );
}

/// `scan3` without Wi-Fi advertises `FoloPassport`. `scan3` runs its Wi-Fi half before its BLE
/// half: with no Wi-Fi module it stops at the `esp_wifi_init` tripwire before any BLE call. A copy
/// whose `wifi_scan3` returns at its entry syncs NimBLE against the virtual controller and puts
/// the AdvData on channels 37, 38 and 39 of each event, then prints its footer with no `FAIL`.
#[test]
fn t1_m8_scan3_without_wifi_advertises_folopassport() {
    let test = "t1_m8_scan3_without_wifi_advertises_folopassport";
    let Some(image) = common::corpus_file_or_skip(test, "scan3", "merged-binary.bin") else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, "scan3", "radio_scan3probe.elf") else {
        return;
    };
    let (flash, elf) = (
        std::fs::read(image).expect("readable"),
        std::fs::read(elf).expect("readable"),
    );
    for executor in BOTH {
        // The Wi-Fi tripwire is reached only with the Wi-Fi module off through `hle.disabled`.
        let mut cfg = MachineConfig::default();
        cfg.hle.disabled = vec!["wifi".to_string()];
        let mut m = machine(&flash, &elf, cfg);
        m.set_executor(executor);
        let out = m.run(one_second());
        assert!(
            matches!(&out.reason, StopReason::Tripwire(r) if r.kind == TripKind::DisabledFeature
                && r.feature == Some("wifi")),
            "{test} {executor:?}: scan3 without the Wi-Fi module stops at esp_wifi_init: {:?}",
            out.reason
        );
        assert_eq!(ble_state(&m).tx_packets, 0, "{test}: no BLE before Wi-Fi");

        let (mut flash, mut elf) = (flash.clone(), elf.clone());
        patch_symbol(&mut flash, &mut elf, "wifi_scan3", 0, 2, |b| {
            b.copy_from_slice(&[0x82, 0x80])
        });
        let mut cfg = MachineConfig::default();
        cfg.hle.disabled = vec!["wifi".to_string()];
        let mut m = machine(&flash, &elf, cfg);
        m.set_executor(executor);
        run_to_line(&mut m, "RC|ble_synced|1", 10_000, test);
        let text = console(&mut m);
        for line in [
            "RC|nimble_port_init|0",
            "EVT|BLE_SYNC|rc=0",
            "RC|adv_start|0",
        ] {
            assert!(text.contains(line), "{test} {executor:?}: no `{line}`");
        }
        assert!(!text.contains("host reset"), "{test} {executor:?}");
        let pdu = on_air(&m).expect("advertising is enabled during the probe's 1 s wait");
        assert_eq!(pdu.adv_data, FOLOPASSPORT_ADV, "{test} {executor:?}");
        run_to_advertising(&mut m, 12_000, test);
        let st = ble_state(&m);
        let event: Vec<_> = st.air.log.iter().rev().take(3).rev().collect();
        assert_eq!(
            event.iter().map(|r| r.channel).collect::<Vec<_>>(),
            air::ADV_CHANNELS,
            "{test} {executor:?}"
        );
        for record in event {
            let on = AdvertisingPdu::from_air(record).expect("an advertising PDU");
            assert_eq!(
                on, pdu,
                "{test} {executor:?}: the PDU the controller advertises"
            );
        }
        // Non-connectable and general discoverable: ADV_SCAN_IND, and the public address from
        // `ble_hs_id_infer_auto(0)`.
        assert_eq!(
            pdu.pdu_type,
            air::pdu::ADV_SCAN_IND,
            "{test} {executor:?}: {pdu:?}"
        );
        assert!(!pdu.random_address, "{test} {executor:?}: {pdu:?}");
        assert_all_answered_ok(&m, test);

        run_to_line(&mut m, "PROBE DONE", 20_000, test);
        let text = console(&mut m);
        assert!(
            text.contains("RC|nimble_port_stop|0"),
            "{test} {executor:?}"
        );
        assert!(
            !text.contains("FAIL"),
            "{test} {executor:?}: {}",
            tail(&mut m)
        );
        assert_eq!(on_air(&m), None, "{test} {executor:?}: advertising stopped");
        let events = ble_state(&m).air.adv_events;
        assert!(events > 0, "{test} {executor:?}");
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 200_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test} {executor:?}");
        assert_eq!(
            ble_state(&m).air.adv_events,
            events,
            "{test} {executor:?}: no advertising event after the disable"
        );
        let st = ble_state(&m);
        assert_eq!(
            st.tx_packets, st.rx_packets,
            "{test}: every packet answered"
        );
    }
}

fn pk_to(flash: &[u8], elf: &[u8], cfg: MachineConfig, executor: Executor, ms: u64) -> Machine {
    let mut m = machine(flash, elf, cfg);
    m.set_executor(executor);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(ms)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{executor:?}: {}",
        tail(&mut m)
    );
    m
}

/// U4 by name. It is also the default; naming it keeps a later default change from making the
/// two-mode tests compare U4 with itself.
fn u4_config() -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.hle.wake = HleWakeMode::U4MagicIsr;
    cfg
}

/// U5 polling, a profile option beside the U4 default.
fn u5_config() -> MachineConfig {
    let mut cfg = MachineConfig::default();
    cfg.hle.wake = HleWakeMode::U5Polling;
    cfg
}

/// On `pk`: NimBLE's startup completes and `pk` advertises its vendor service (the
/// 128-bit UUID, `Passport Keys` in the scan response, 20 ms interval, connectable) with no host
/// reset, in U5 and U4, and at 3 s both executors agree on state hash, console and BLE state.
#[test]
fn t1_m8_pk_advertises_and_both_executors_agree_after_ble_startup() {
    let test = "t1_m8_pk_advertises_and_both_executors_agree_after_ble_startup";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    // `12D4FA08-7418-48FA-A95A-B43A2E669E55`, least significant octet first.
    let uuid = [
        0x55, 0x9E, 0x66, 0x2E, 0x3A, 0xB4, 0x5A, 0xA9, 0xFA, 0x48, 0x18, 0x74, 0x08, 0xFA, 0xD4,
        0x12,
    ];
    for (mode, cfg) in [("U5", u5_config()), ("U4", u4_config())] {
        let mut runs: Vec<Machine> = BOTH
            .iter()
            .map(|e| pk_to(&flash, &elf, cfg.clone(), *e, 3_000))
            .collect();
        for m in runs.iter_mut() {
            let text = console(m);
            assert!(
                text.contains("main_task: Returned from app_main()"),
                "{test} {mode}"
            );
            assert!(!text.contains("host reset"), "{test} {mode}: {}", tail(m));
            assert_all_answered_ok(m, test);
            let st = ble_state(m);
            assert_eq!(st.tx_packets, st.rx_packets, "{test} {mode}");
            let pdu = on_air(m).expect("pk advertises");
            assert_eq!(pdu.pdu_type, air::pdu::ADV_IND, "{test} {mode}");
            assert_eq!(&pdu.adv_data[..3], [0x02, 0x01, 0x06], "{test} {mode}");
            assert_eq!(&pdu.adv_data[3..5], [0x11, 0x07], "{test} {mode}");
            assert_eq!(pdu.adv_data[5..21], uuid, "{test} {mode}");
            assert_eq!(
                &st.controller.advertising.scan_response[2..],
                b"Passport Keys"
            );
            assert_eq!(
                st.controller.advertising.interval(),
                (0x20, 0x20),
                "{test} {mode}"
            );
        }
        let (b, a) = (runs.pop().unwrap(), runs.pop().unwrap());
        assert_eq!(
            a.state_hash(),
            b.state_hash(),
            "{test} {mode}: executors differ"
        );
        assert_eq!(
            a.radio_module_state("ble"),
            b.radio_module_state("ble"),
            "{test} {mode}"
        );
        let (mut a, mut b) = (a, b);
        assert_eq!(console(&mut a), console(&mut b), "{test} {mode}");
    }
}

/// A snapshot taken between an HCI command and its answer restores in a fresh machine and
/// continues to the same console, BLE state and state hash, in U5 and U4 on both executors.
#[test]
fn t1_m8_a_snapshot_mid_hci_exchange_restores_identically() {
    let test = "t1_m8_a_snapshot_mid_hci_exchange_restores_identically";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for (mode, cfg) in [("U5", u5_config()), ("U4", u4_config())] {
        for executor in BOTH {
            let mut m = machine(&flash, &elf, cfg.clone());
            m.set_executor(executor);
            // 20 us steps: an HCI command is answered 74 us after it is sent (`reply_us` of ble.toml), which
            // a 100 us step steps over.
            let mut exchanges = 0;
            loop {
                assert!(
                    m.now() < VTime::from_ms(3_000),
                    "{test} {mode}: no exchange"
                );
                let out = m.run(RunLimits {
                    until: Some(VTime::from_us(m.now().as_us() + 20)),
                    max_insns: None,
                    stops: StopSet::default(),
                });
                assert_eq!(out.reason, StopReason::Until, "{test} {mode}");
                if !ble_state(&m).outbox.is_empty() {
                    exchanges += 1;
                    // The fifth command: past HCI Reset, inside the startup sequence.
                    if exchanges == 5 {
                        break;
                    }
                    while !ble_state(&m).outbox.is_empty() {
                        m.run(RunLimits {
                            until: Some(VTime::from_us(m.now().as_us() + 20)),
                            max_insns: None,
                            stops: StopSet::default(),
                        });
                    }
                }
            }
            let at = ble_state(&m);
            let cursor = m.io().serial_ring(SerialStream::UsjTx).head();
            let bytes = m
                .snapshot(SnapOpts::default())
                .to_bytes()
                .expect("a machine snapshot serializes");
            let mut restored = machine(&flash, &elf, cfg.clone());
            restored.set_executor(executor);
            restored
                .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
                .expect("restores");
            assert_eq!(
                restored.state_hash(),
                m.state_hash(),
                "{test} {mode} {executor:?}"
            );
            assert_eq!(ble_state(&restored), at, "{test} {mode}: the queued answer");
            let end = || RunLimits {
                until: Some(VTime::from_ms(3_000)),
                max_insns: None,
                stops: StopSet::default(),
            };
            let (x, y) = (m.run(end()), restored.run(end()));
            assert_eq!(
                (x.reason, x.insns),
                (y.reason, y.insns),
                "{test} {mode} {executor:?}"
            );
            assert_eq!(
                restored.state_hash(),
                m.state_hash(),
                "{test} {mode} {executor:?}"
            );
            let after = |m: &mut Machine| -> Vec<u8> {
                let ring = m.io().serial_ring(SerialStream::UsjTx);
                ring.slices(cursor).iter().copied().collect()
            };
            assert_eq!(
                after(&mut restored),
                after(&mut m),
                "{test} {mode} {executor:?}"
            );
            let done = ble_state(&restored);
            assert_eq!(done, ble_state(&m), "{test} {mode} {executor:?}");
            assert!(done.rx_packets > at.rx_packets + 1, "{test} {mode}");
            assert!(on_air(&restored).is_some(), "{test} {mode}: advertising");
        }
    }
}

/// On `official`: entering the BLE card (the sixth of seven) starts NimBLE, and the demo
/// advertises the `FoloPassport` AdvData at the 100 to 150 ms interval NimBLE picks for intervals
/// left 0, with every command answered, on both executors.
#[test]
fn t1_m8_official_ble_card_advertises_folopassport() {
    use pemu_core::input::{ButtonId, InputEvent};
    use pemu_machine::machine::At;
    let test = "t1_m8_official_ble_card_advertises_folopassport";
    let Some(image) =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")
    else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport.elf")
    else {
        return;
    };
    let (flash, elf) = (
        std::fs::read(image).expect("readable"),
        std::fs::read(elf).expect("readable"),
    );
    let mut hashes = Vec::new();
    for executor in BOTH {
        let mut m = machine(&flash, &elf, MachineConfig::default());
        m.set_executor(executor);
        let keys = [ButtonId::Down; 5].into_iter().chain([ButtonId::Ok]);
        for (i, id) in keys.enumerate() {
            let t = 1_000 + 500 * i as u64;
            for (dt, down) in [(0, true), (120, false)] {
                m.input(
                    At::Vt(VTime::from_ms(t + dt)),
                    InputEvent::Button { id, down },
                )
                .expect("a future input journals");
            }
        }
        let out = m.run(RunLimits {
            until: Some(VTime::from_ms(6_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(
            out.reason,
            StopReason::Until,
            "{test} {executor:?}: {}",
            tail(&mut m)
        );
        let text = console(&mut m);
        assert!(
            text.contains("NimBLE: GAP procedure initiated: advertise"),
            "{test}: {}",
            tail(&mut m)
        );
        assert!(!text.contains("host reset"), "{test}");
        assert_all_answered_ok(&m, test);
        let pdu = on_air(&m).expect("the BLE card advertises");
        assert_eq!(pdu.adv_data, FOLOPASSPORT_ADV, "{test} {executor:?}");
        assert!(!pdu.random_address, "{test}");
        let st = ble_state(&m);
        assert_eq!(
            st.controller.advertising.interval(),
            (0xA0, 0xF0),
            "{test} {executor:?}"
        );
        hashes.push(m.state_hash());
    }
    assert_eq!(hashes[0], hashes[1], "{test}: executors differ");
}

fn queued_le_rand(m: &Machine) -> Option<Vec<u8>> {
    ble_state(m).outbox.iter().find_map(|out| {
        let p = &out.packet;
        (p.len() == 15 && p[..7] == [0x04, 0x0E, 0x0C, 0x05, 0x18, 0x20, 0x00])
            .then(|| p[7..].to_vec())
    })
}

/// Runs `m` 100 us at a time until the `HCI_LE_Rand` answer is queued, snapshotting at the first
/// queued answer (HCI Reset); returns the random octets and the snapshot bytes.
fn run_to_le_rand(m: &mut Machine, test: &str) -> (Vec<u8>, Option<Vec<u8>>) {
    let mut early = None;
    loop {
        assert!(m.now() < VTime::from_ms(5_000), "{test}: no LE Rand");
        m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 100)),
            max_insns: None,
            stops: StopSet::default(),
        });
        if let Some(octets) = queued_le_rand(m) {
            return (octets, early);
        }
        if early.is_none() && !ble_state(m).outbox.is_empty() {
            early = Some(
                m.snapshot(SnapOpts::default())
                    .to_bytes()
                    .expect("a machine snapshot serializes"),
            );
        }
    }
}

/// `HCI_LE_Rand` is answered from `RngStream::RADIO_BLE`: the same seed gives the same octets,
/// another seed others, and a restore from before the command draws the same octets.
#[test]
fn t1_m8_le_rand_follows_the_machine_seed_and_survives_a_restore() {
    use pemu_core::rng::{DetRng, RngStream};
    let test = "t1_m8_le_rand_follows_the_machine_seed_and_survives_a_restore";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let seeded = |seed: u64| MachineConfig {
        seed,
        ..MachineConfig::default()
    };
    let mut a = machine(&flash, &elf, seeded(1));
    let (first, early) = run_to_le_rand(&mut a, test);
    let early = early.expect("HCI Reset is answered before LE Rand");
    assert_eq!(first.len(), 8, "{test}");
    let after = a.snapshot(SnapOpts::default());
    let rng: DetRng = after.get().expect("the rng section decodes");
    assert_eq!(
        rng.pos_of(RngStream::RADIO_BLE),
        2,
        "{test}: one 8-octet draw"
    );

    let mut again = machine(&flash, &elf, seeded(1));
    assert_eq!(
        run_to_le_rand(&mut again, test).0,
        first,
        "{test}: same seed"
    );
    let mut other = machine(&flash, &elf, seeded(2));
    assert_ne!(
        run_to_le_rand(&mut other, test).0,
        first,
        "{test}: another seed"
    );

    let snapshot = Snapshot::from_bytes(&early).expect("parses");
    let before: DetRng = snapshot.get().expect("the rng section decodes");
    assert_eq!(
        before.pos_of(RngStream::RADIO_BLE),
        0,
        "{test}: taken before the draw"
    );
    let mut restored = machine(&flash, &elf, seeded(1));
    restored.restore(&snapshot).expect("restores");
    assert_eq!(
        run_to_le_rand(&mut restored, test).0,
        first,
        "{test}: restored"
    );
}

// ---------------------------------------------------------------------------------------------
// The full `pk` boot against the derived device golden
// ---------------------------------------------------------------------------------------------

/// Virtual time the device ran before esptool reset it into the captured boot.
const DEVICE_LIKE_MS: u64 = 300;

/// Lines of the derived device golden the boot test claims: dev:L4 to dev:L80, the whole body of
/// `pk.console.txt`.
const PK_BOOT_LINES: usize = 77;

/// dev:L80 (`pk_app.c` `refresh_link_state`).
const PK_BOOT_LAST: &str = "pk_app: link state -1 -> 0";

/// The device-like line reset with the U3 client stated: power on, run 300 ms, open the CDC-ACM
/// port (which paces the console), then `UsbLine {rts: 1, dtr: 0}`, the esptool hard reset that
/// made the captured boot `rst:0x15`.
fn device_like_reset_with_a_client(m: &mut Machine, test: &str) {
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(DEVICE_LIKE_MS)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(
        out.reason,
        StopReason::Until,
        "{test}: the first boot ends before {DEVICE_LIKE_MS} ms at pc {:#010x}",
        m.hart().pc
    );
    m.input(At::Now, InputEvent::UsbClient { open: true })
        .expect("now is not in the past");
    assert!(
        m.io().usj_ctrl.cable() && m.io().usj_ctrl.client_open(),
        "{test}: the cable is plugged and the client has the port open (U3) before the reset"
    );
    m.input(
        At::Now,
        InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
    )
    .expect("now is not in the past");
}

/// `pk` with a client open (U3): the normalized last boot equals dev:L4 to dev:L80 of the derived
/// device golden; without the BLE module `pk` stops at the `esp_bt_controller_init` tripwire.
///
/// Text rule only: timestamps become `(T)` and no mask or band is added. The golden is derived on
/// this host and never committed, so the test skips with the loader's reason where it is absent.
#[test]
fn t1_m8_pk_boot_with_a_client_open_equals_the_device_golden() {
    let test = "t1_m8_pk_boot_with_a_client_open_equals_the_device_golden";
    let id = test.to_string();
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let Some(golden) = common::derived_golden_or_skip(test, "pk.console.txt") else {
        return;
    };

    let mut m = machine(&flash, &elf, MachineConfig::default());
    device_like_reset_with_a_client(&mut m, test);
    run_to_line(&mut m, PK_BOOT_LAST, DEVICE_LIKE_MS + 10_000, test);

    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let console: Vec<u8> = ring.slices(0).iter().copied().collect();
    let compared =
        common::assert_console_prefix("pk.console.txt", &golden, &console, Some(PK_BOOT_LINES));
    assert_eq!(compared, PK_BOOT_LINES, "{id}");

    // The named lines, read out of the prefix just proved equal, so a missing one is named.
    let text = &golden.lines()[..PK_BOOT_LINES];
    for named in [
        "I (T) BLE_INIT: BT controller compile version ",
        "I (T) BLE_INIT: Using main XTAL as clock source",
        "I (T) BLE_INIT: Feature Config, ADV:1",
        "I (T) BLE_INIT: Bluetooth MAC: <MAC>",
        "I (T) phy_init: phy_version 1232,d493f299,Aug 25 2025,19:01:20",
        "I (T) pk_app: ready: boot=<masked> ui=1 battery=1",
        "I (T) adc_button: ADC1 has been initialized",
        "I (T) adc_button: calibration scheme version is Curve Fitting",
        "I (T) adc_button: Calibration Success",
        "I (T) button: IoT Button Version: 4.2.0",
        "I (T) main_task: Returned from app_main()",
        "I (T) pk_app: link state -1 -> 0",
    ] {
        assert!(
            text.iter().any(|l| l.starts_with(named)),
            "{id}: the compared prefix has no line starting `{named}`"
        );
    }
    assert_eq!(
        text.iter()
            .filter(|l| l.starts_with("I (T) BLE_INIT: "))
            .count(),
        4,
        "{id}: the row claims 4 `BLE_INIT` lines"
    );
    println!("RAN {test}: {PK_BOOT_LINES} lines (dev:L4-L80) equal the derived device golden");
}

// ---------------------------------------------------------------------------------------------
// The virtual air, the scripted central and the btsnoop capture
// ---------------------------------------------------------------------------------------------

use pemu_radio::ble::btsnoop;
use pemu_radio::ble::central::{self, Step, StepStatus, Target, Uuid};

/// The Passport Keys GATT service and its two characteristics (`pk_ble.c` header).
const PK_SERVICE: &str = "12D4FA08-7418-48FA-A95A-B43A2E669E55";
const PK_EVENTS: &str = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
const PK_COMMANDS: &str = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";

fn uuid(text: &str) -> Uuid {
    Uuid::parse(text).expect("a UUID")
}

/// The GATT exchange as a central script: connect to the vendor service, exchange MTU 247 (the
/// reference central's), discover, subscribe, then `{"cmd":"hello"}` and
/// `{"cmd":"ping"}`. Each line ends with `\n`, which `pk_line_feed` frames on.
fn gatt_script() -> Vec<Step> {
    let write = |text: &str| Step::Write {
        characteristic: uuid(PK_COMMANDS),
        value: format!("{text}\n").into_bytes(),
        with_response: true,
    };
    let wait = |text: &str| Step::WaitNotification {
        characteristic: uuid(PK_EVENTS),
        contains: text.as_bytes().to_vec(),
        within_ms: 5_000,
    };
    vec![
        Step::Scan { ms: 200 },
        Step::Connect {
            target: Target::Service(uuid(PK_SERVICE)),
            // 30 ms, no latency, 4 s: class C parameters of this central.
            interval: 24,
            latency: 0,
            timeout: 400,
            within_ms: 2_000,
        },
        Step::ExchangeMtu { mtu: 247 },
        Step::DiscoverServices,
        Step::DiscoverCharacteristics {
            service: uuid(PK_SERVICE),
        },
        Step::Subscribe {
            characteristic: uuid(PK_EVENTS),
            indicate: false,
        },
        write("{\"cmd\":\"hello\"}"),
        wait("\"t\":\"hello\""),
        write("{\"cmd\":\"ping\"}"),
        wait("{\"t\":\"pong\"}"),
    ]
}

fn journal_script(m: &mut Machine, steps: &[Step]) {
    use pemu_core::input::{EnvChange, InputEvent};
    use pemu_machine::machine::At;
    m.input(
        At::Now,
        InputEvent::Env(EnvChange::BleCentral {
            script: central::encode_script(steps),
        }),
    )
    .expect("a central script journals");
}

fn run_to_advertising(m: &mut Machine, limit_ms: u64, test: &str) {
    while ble_state(m).air.adv_events == 0 {
        assert!(
            m.now() < VTime::from_ms(limit_ms),
            "{test}: nothing advertised by {limit_ms} ms: {}",
            tail(m)
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 1_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test}: {}", tail(m));
    }
}

fn run_script(m: &mut Machine, steps: usize, limit_ms: u64, test: &str) {
    loop {
        let st = ble_state(m);
        if st.central.running.is_none()
            && st.central.queue.is_empty()
            && st.central.results.len() >= steps
        {
            return;
        }
        assert!(
            m.now() < VTime::from_ms(limit_ms),
            "{test}: the script did not finish by {limit_ms} ms: {:?}\n{}",
            st.central.results,
            tail(m)
        );
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 10_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test}: {}", tail(m));
    }
}

fn notified_text(st: &BleState) -> String {
    let handle = st
        .central
        .characteristics
        .iter()
        .find(|c| c.uuid == uuid(PK_EVENTS))
        .map(|c| c.value_handle);
    let bytes: Vec<u8> = st
        .central
        .notifications
        .iter()
        .filter(|n| Some(n.handle) == handle)
        .flat_map(|n| n.value.iter().copied())
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The checks of a finished exchange on `m`: every step `Ok`, the advertising and scan response
/// found, the answers arrived, no host reset, every command answered, and the btsnoop capture
/// parses and holds the two writes, the answers and completed-packets events.
fn assert_exchange(m: &mut Machine, test: &str) {
    let st = ble_state(m);
    let statuses: Vec<StepStatus> = st.central.results.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        vec![StepStatus::Ok; gatt_script().len()],
        "{test}: {:?}",
        st.central.results
    );
    let report = st
        .central
        .reports
        .iter()
        .find(|r| central::ad_lists_service(&r.data, uuid(PK_SERVICE)))
        .expect("the advertising of the vendor service was found");
    assert_eq!(report.pdu_type, air::pdu::ADV_IND, "{test}: connectable");
    assert_eq!(report.address, st.public_address(), "{test}");
    assert_eq!(
        report.scan_response.as_deref().map(|s| &s[2..]),
        Some(&b"Passport Keys"[..]),
        "{test}"
    );
    assert_eq!(st.central.link.as_ref().map(|l| l.mtu), Some(247), "{test}");
    let text = notified_text(&st);
    assert!(text.contains("\"t\":\"hello\""), "{test}: {text}");
    assert!(text.contains("{\"t\":\"pong\"}"), "{test}: {text}");
    let console = console(m);
    assert!(!console.contains("host reset"), "{test}: {}", tail(m));
    assert_all_answered_ok(m, test);
    assert_eq!(st.controller.acl_dropped, 0, "{test}");

    let file = st.capture.to_btsnoop();
    let packets = btsnoop::parse(&file).expect("the btsnoop capture parses");
    assert_eq!(
        st.capture.dropped, 0,
        "{test}: the whole boot fits the capture"
    );
    assert_eq!(packets.len(), st.capture.records.len(), "{test}");
    let has = |flags: u32, needle: &[u8]| {
        packets
            .iter()
            .any(|p| p.flags == flags && p.packet.windows(needle.len()).any(|w| w == needle))
    };
    // Flags: bit 0 received by the host, bit 1 command or event (btsnoop).
    assert!(
        has(0b01, b"{\"cmd\":\"hello\"}"),
        "{test}: the write reached the host"
    );
    assert!(has(0b01, b"{\"cmd\":\"ping\"}"), "{test}");
    assert!(
        has(0b00, b"\"t\":\"hello\""),
        "{test}: the host's notification"
    );
    assert!(has(0b00, b"{\"t\":\"pong\"}"), "{test}");
    assert!(
        packets
            .iter()
            .any(|p| p.flags == 0b11 && p.packet[..2] == [0x04, 0x13]),
        "{test}: Number Of Completed Packets"
    );
    assert!(
        packets.windows(2).all(|w| w[0].unix_us <= w[1].unix_us),
        "{test}: virtual instants in order"
    );
}

/// On `pk`: the virtual central connects, subscribes, and gets `"t":"hello"` and `{"t":"pong"}`,
/// with no host reset and a btsnoop capture that parses, on both executors, which agree on the
/// state hash, console and BLE state. The script starts after `pk_app: ready` and the first
/// advertising event.
#[test]
fn t1_m8_pk_gatt_hello_and_ping_on_the_virtual_air() {
    let test = "t1_m8_pk_gatt_hello_and_ping_on_the_virtual_air";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut runs = Vec::new();
    for executor in BOTH {
        let mut m = machine(&flash, &elf, MachineConfig::default());
        m.set_executor(executor);
        run_to_line(&mut m, "pk_app: ready", 5_000, test);
        run_to_advertising(&mut m, 5_000, test);
        journal_script(&mut m, &gatt_script());
        run_script(&mut m, gatt_script().len(), 15_000, test);
        assert_exchange(&mut m, test);
        runs.push(m);
    }
    let (mut b, mut a) = (runs.pop().expect("two"), runs.pop().expect("two"));
    assert_eq!(
        a.now(),
        b.now(),
        "{test}: the exchange ended at the same step"
    );
    assert_eq!(a.state_hash(), b.state_hash(), "{test}: executors differ");
    assert_eq!(
        a.radio_module_state("ble"),
        b.radio_module_state("ble"),
        "{test}"
    );
    assert_eq!(console(&mut a), console(&mut b), "{test}");
}

/// On `pkgatt` (the GATT probe linking `pk_ble.c` and `pk_protocol.c`): the same exchange gives the
/// console lines of the reference run, timestamps excluded. The probe also sends a button frame on
/// subscribe, which arrives before the hello answer.
#[test]
fn t1_m8_pkgatt_console_matches_g2_gt3() {
    let test = "t1_m8_pkgatt_console_matches_g2_gt3";
    let Some(image) = common::corpus_file_or_skip(test, "pkgatt", "merged-binary.bin") else {
        return;
    };
    let Some(elf) = common::corpus_file_or_skip(test, "pkgatt", "radio_pkgatt.elf") else {
        return;
    };
    let (flash, elf) = (
        std::fs::read(image).expect("readable"),
        std::fs::read(elf).expect("readable"),
    );
    let mut hashes = Vec::new();
    for executor in BOTH {
        let mut m = machine(&flash, &elf, MachineConfig::default());
        m.set_executor(executor);
        run_to_line(&mut m, "RC|pk_ble_start|0", 5_000, test);
        run_to_advertising(&mut m, 5_000, test);
        journal_script(&mut m, &gatt_script());
        run_script(&mut m, gatt_script().len(), 15_000, test);
        assert_exchange(&mut m, test);
        run_to_line(&mut m, "PROBE DONE", 20_000, test);
        let text = console(&mut m);
        // The reference run's lines with their `|t_ms=` suffix removed.
        let lines: Vec<&str> = text
            .lines()
            .map(|l| l.split("|t_ms=").next().unwrap_or(l))
            .filter(|l| {
                l.starts_with("EVT|LINK") || l.starts_with("RX|") || l.starts_with("RC|gatt")
            })
            .collect();
        assert_eq!(
            lines,
            [
                "EVT|LINK|subscribed=1",
                "RX|1|{\"cmd\":\"hello\"}",
                "RX|2|{\"cmd\":\"ping\"}",
                "RC|gatt|subscribed=1|lines=2|tx=3",
            ],
            "{test} {executor:?}: {}",
            tail(&mut m)
        );
        let st = ble_state(&m);
        assert!(
            notified_text(&st).contains("\"t\":\"btn\""),
            "{test}: the subscribe frame"
        );
        hashes.push(m.state_hash());
    }
    assert_eq!(hashes[0], hashes[1], "{test}: executors differ");
}

/// A snapshot taken while an ATT request of the central is in flight restores in a fresh machine
/// and continues to the same console, BLE state and state hash, in U5 and U4 on both executors;
/// and a fresh machine journaling the same inputs replays to the same state.
#[test]
fn t1_m8_a_snapshot_inside_the_gatt_exchange_restores_and_the_journal_replays() {
    use pemu_machine::machine::At;
    let test = "t1_m8_a_snapshot_inside_the_gatt_exchange_restores_and_the_journal_replays";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for (mode, cfg) in [("U5", u5_config()), ("U4", u4_config())] {
        for executor in BOTH {
            let mut m = machine(&flash, &elf, cfg.clone());
            m.set_executor(executor);
            run_to_line(&mut m, "pk_app: ready", 5_000, test);
            run_to_advertising(&mut m, 5_000, test);
            journal_script(&mut m, &gatt_script());
            loop {
                let st = ble_state(&m);
                let pending = st
                    .central
                    .link
                    .as_ref()
                    .is_some_and(|l| l.pending.is_some());
                if st.central.results.len() >= 7 && pending {
                    break;
                }
                assert!(
                    m.now() < VTime::from_ms(15_000),
                    "{test} {mode}: never in flight"
                );
                let out = m.run(RunLimits {
                    until: Some(VTime::from_us(m.now().as_us() + 1_000)),
                    max_insns: None,
                    stops: StopSet::default(),
                });
                assert_eq!(out.reason, StopReason::Until, "{test} {mode}");
            }
            let at = ble_state(&m);
            let cursor = m.io().serial_ring(SerialStream::UsjTx).head();
            let bytes = m
                .snapshot(SnapOpts::default())
                .to_bytes()
                .expect("a machine snapshot serializes");
            let mut restored = machine(&flash, &elf, cfg.clone());
            restored.set_executor(executor);
            restored
                .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
                .expect("restores");
            assert_eq!(
                restored.state_hash(),
                m.state_hash(),
                "{test} {mode} {executor:?}"
            );
            assert_eq!(
                ble_state(&restored),
                at,
                "{test} {mode}: the central mid-step"
            );
            let end = VTime::from_us(m.now().as_us() + 4_000_000);
            let limits = || RunLimits {
                until: Some(end),
                max_insns: None,
                stops: StopSet::default(),
            };
            let (x, y) = (m.run(limits()), restored.run(limits()));
            assert_eq!(
                (x.reason, x.insns),
                (y.reason, y.insns),
                "{test} {mode} {executor:?}"
            );
            assert_eq!(
                restored.state_hash(),
                m.state_hash(),
                "{test} {mode} {executor:?}"
            );
            let after = |m: &mut Machine| -> Vec<u8> {
                let ring = m.io().serial_ring(SerialStream::UsjTx);
                ring.slices(cursor).iter().copied().collect()
            };
            assert_eq!(
                after(&mut restored),
                after(&mut m),
                "{test} {mode} {executor:?}"
            );
            assert_eq!(
                ble_state(&restored),
                ble_state(&m),
                "{test} {mode} {executor:?}"
            );
            assert_exchange(&mut restored, test);

            if executor == Executor::Engine {
                let mut replay = machine(&flash, &elf, cfg.clone());
                replay.set_executor(executor);
                for entry in m.journal().entries() {
                    replay
                        .input(At::Vt(entry.at), entry.ev.clone())
                        .expect("a recorded input journals again");
                }
                let out = replay.run(limits());
                assert_eq!(out.reason, StopReason::Until, "{test} {mode}");
                assert_eq!(replay.state_hash(), m.state_hash(), "{test} {mode}: replay");
                assert_eq!(ble_state(&replay), ble_state(&m), "{test} {mode}: replay");
            }
        }
    }
}

/// With the BLE module disabled a central script is counted unapplied and nothing reaches the
/// air; a script the bound module cannot decode is refused the same way.
#[test]
fn t1_m8_a_central_script_without_a_bound_module_or_that_does_not_decode_is_unapplied() {
    use pemu_core::input::{EnvChange, InputEvent};
    use pemu_machine::machine::At;
    let test = "t1_m8_a_central_script_without_a_bound_module_or_that_does_not_decode_is_unapplied";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut m = machine(&flash, &elf, ble_disabled());
    let before = m.unapplied_inputs();
    journal_script(&mut m, &[Step::Scan { ms: 10 }]);
    m.run(RunLimits {
        until: Some(VTime::from_ms(50)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(m.unapplied_inputs(), before + 1, "{test}: no ble module");
    assert_eq!(m.radio_module_state("ble"), None, "{test}");

    let mut m = machine(&flash, &elf, MachineConfig::default());
    run_to_advertising(&mut m, 5_000, test);
    let state = ble_state(&m);
    let before = m.unapplied_inputs();
    m.input(
        At::Now,
        InputEvent::Env(EnvChange::BleCentral {
            script: vec![central::SCRIPT_VERSION + 1],
        }),
    )
    .expect("journals");
    m.run(RunLimits {
        until: Some(VTime::from_us(m.now().as_us() + 1)),
        max_insns: None,
        stops: StopSet::default(),
    });
    assert_eq!(m.unapplied_inputs(), before + 1, "{test}: refused script");
    assert_eq!(ble_state(&m).central, state.central, "{test}");
}

/// A redacted export after the GATT exchange carries no key material in the BLE module state:
/// the `HCI_LE_Rand` output is zero, redacting again changes nothing, and no SMP payload
/// survives; the application's ATT payloads are not secret and stay.
#[test]
fn t1_m8_a_redacted_export_holds_no_key_material_of_the_ble_module() {
    let test = "t1_m8_a_redacted_export_holds_no_key_material_of_the_ble_module";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut m = machine(&flash, &elf, MachineConfig::default());
    run_to_line(&mut m, "pk_app: ready", 5_000, test);
    run_to_advertising(&mut m, 5_000, test);
    journal_script(&mut m, &gatt_script());
    run_script(&mut m, gatt_script().len(), 15_000, test);
    let export = m.snapshot(SnapOpts {
        export: true,
        include_secrets: false,
    });
    assert!(export.header.redacted, "{test}: the export is redacted");
    let id = pemu_core::snap::SectionId::new(pemu_machine::hle::HLE_MACHINE);
    let section = export
        .section(&id)
        .expect("the module section is in the export");
    let hle: pemu_machine::hle::HleMachineSection = pemu_core::snap::serde_from_section(
        section,
        id,
        pemu_machine::snapshot::SECTION_VERSION,
        "hle.machine",
    )
    .expect("the hle.machine section decodes");
    let st = BleState::decode(
        hle.modules
            .get("ble")
            .map(Vec::as_slice)
            .expect("the ble module state is in the export"),
    )
    .expect("the module state decodes");
    // Not vacuous: the firmware did ask the controller for key material.
    assert!(
        st.capture.redacted > 0,
        "{test}: no packet carried key material"
    );
    let mut rands = 0;
    for record in &st.capture.records {
        let mut again = record.packet.clone();
        btsnoop::redact(&mut again);
        assert_eq!(again, record.packet, "{test}: key material is stored");
        // The `HCI_LE_Rand` return parameters (Core Vol 4 Part E 7.8.23): status then 8 octets.
        if record.packet.len() == 15
            && record.packet[..2] == [0x04, 0x0E]
            && record.packet[4..6] == [0x18, 0x20]
        {
            rands += 1;
            assert_eq!(
                record.packet[7..15],
                [0; 8],
                "{test}: a random number leaks"
            );
        }
        // No SMP payload (L2CAP CID 6) beyond its opcode.
        if record.packet.first() == Some(&0x02)
            && record.packet.len() > 9
            && record.packet[7..9] == [0x06, 0x00]
        {
            assert!(
                record.packet[10..].iter().all(|b| *b == 0),
                "{test}: an SMP payload leaks"
            );
        }
    }
    assert!(rands > 0, "{test}: the run held no LE_Rand answer");
    let stored: Vec<u8> = st
        .capture
        .records
        .iter()
        .flat_map(|r| r.packet.clone())
        .collect();
    assert!(
        stored.windows(15).any(|w| w == b"{\"cmd\":\"hello\"}"),
        "{test}: the application payloads are kept"
    );
}

// ---------------------------------------------------------------------------------------------
// The heap ledger and the journaled external HCI door
// ---------------------------------------------------------------------------------------------

/// Bytes the `[[heap]]` plan of `ble.toml` holds on `pk` and `pkgatt` once the controller is
/// enabled, stated rather than summed so a row change has to change it on purpose: the seven
/// named rows (5030) and the six campaign-measured rows (22528 + 672 + 108 at init, 88 + 96 that
/// outlive deinit, 32 from enable to disable).
const PK_LEDGER_BYTES: u32 = 28_554;
const PK_LEDGER_BLOCKS: usize = 13;

fn ledger(m: &Machine) -> Vec<pemu_machine::hle::HleHeapBlock> {
    m.heap_ledger()
}

/// `esp_bt_controller_init` takes the `[[heap]]` plan of `ble.toml` out of the guest's own
/// allocator, so the guest's heap carries what the replaced controller blob would have held.
/// Every block is labelled with its row and class, the fidelity is `blob_allocations_estimated`,
/// the blocks are distinct guest addresses, and the free heap falls by at least the ledger.
#[test]
fn t1_m8_the_heap_ledger_holds_its_plan_on_pk_with_labels_and_classes() {
    let test = "t1_m8_the_heap_ledger_holds_its_plan_on_pk_with_labels_and_classes";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for executor in BOTH {
        let mut m = machine(&flash, &elf, MachineConfig::default());
        m.set_executor(executor);
        // Nothing is held before BLE init.
        assert!(ledger(&m).is_empty(), "{test} {executor:?}: held too early");
        run_to_line(&mut m, "pk_app: ready", 5_000, test);
        let blocks = ledger(&m);
        assert_eq!(
            blocks.len(),
            PK_LEDGER_BLOCKS,
            "{test} {executor:?}: {blocks:?}"
        );
        let labels: Vec<&str> = blocks.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "acl_rx",
                "adv_data",
                "accept_list",
                "resolving_list",
                "link_env",
                "scan_dupl",
                "adv_dup_filt",
                "blob_init",
                "blob_init_2",
                "blob_init_3",
                "blob_retained",
                "blob_retained_tail",
                "blob_enable",
            ],
            "{test} {executor:?}"
        );
        let classes: Vec<&str> = blocks.iter().map(|b| b.class.as_str()).collect();
        assert_eq!(
            classes,
            [
                "A", "A", "A", "A", "C", "C", "C", "B", "B", "B", "B", "B", "B"
            ],
            "{test} {executor:?}: the four capture-derived counts are class A, the six rows \
             measured by the campaign capture class B"
        );
        let bytes: u32 = blocks.iter().map(|b| b.bytes).sum();
        assert_eq!(bytes, PK_LEDGER_BYTES, "{test} {executor:?}");
        for block in &blocks {
            assert_eq!(block.module, "ble", "{test}");
            assert_eq!(
                block.fidelity, "blob_allocations_estimated",
                "{test}: the receipt word"
            );
            assert_ne!(block.addr, 0, "{test}: {} was not allocated", block.label);
            assert!(
                (0x3fc0_0000..0x3fd0_0000).contains(&block.addr),
                "{test}: {} at {:#010x} is not guest DRAM",
                block.label,
                block.addr
            );
        }
        let mut addrs: Vec<u32> = blocks.iter().map(|b| b.addr).collect();
        addrs.sort_unstable();
        addrs.dedup();
        assert_eq!(addrs.len(), blocks.len(), "{test}: blocks overlap");
        assert_eq!(
            ble_state(&m).ledger.refused,
            0,
            "{test}: the allocator refused nothing"
        );
    }
}

/// The real `inspect heap` command over a `pk` session labels every ledger block with its module,
/// row, class and fidelity, in JSON and text, and each block lies inside a region the TLSF walker
/// found.
#[test]
fn t1_m8_inspect_heap_labels_the_ledger_blocks_on_pk() {
    let test = "t1_m8_inspect_heap_labels_the_ledger_blocks_on_pk";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": common::PK}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    let ready = call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/pk_app: ready/", "timeout": "10s"}),
    );
    assert_eq!(
        ready.json["status"], "matched",
        "{test}: pk must reach its ready line: {}",
        ready.text
    );
    let out = call(
        "inspect",
        serde_json::json!({"instance": id, "what": ["heap"]}),
    );
    let heap = &out.json["heap"];
    let ledger = &heap["ledger"];
    assert_eq!(
        ledger["fidelity"], "blob_allocations_estimated",
        "{test}: the heap fidelity declaration: {}",
        out.text
    );
    assert_eq!(ledger["bytes"], PK_LEDGER_BYTES, "{test}: {}", out.text);
    let blocks = ledger["blocks"].as_array().expect("a block list");
    assert_eq!(blocks.len(), PK_LEDGER_BLOCKS, "{test}: {}", out.text);
    let regions = heap["regions"].as_array().expect("the walked regions");
    assert!(!regions.is_empty(), "{test}: the walker found no region");
    let bound = |key: &str, region: &serde_json::Value| -> u32 {
        u32::from_str_radix(
            region[key]
                .as_str()
                .expect("a region bound")
                .trim_start_matches("0x"),
            16,
        )
        .expect("hex")
    };
    for block in blocks {
        assert_eq!(block["module"], "ble", "{test}");
        let label = block["label"].as_str().expect("a label");
        assert!(!label.is_empty(), "{test}: an unlabelled ledger block");
        assert!(
            ["A", "B", "C"].contains(&block["class"].as_str().expect("a class")),
            "{test}: {label} has no fidelity class"
        );
        let addr = u32::from_str_radix(
            block["addr"]
                .as_str()
                .expect("an address")
                .trim_start_matches("0x"),
            16,
        )
        .expect("hex");
        assert!(
            regions
                .iter()
                .any(|r| (bound("start", r)..bound("end", r)).contains(&addr)),
            "{test}: {label} at {addr:#010x} is outside every walked heap region: {}",
            out.text
        );
    }
    // The receipt declares the approximation, so a reader of it alone knows these bytes are class C.
    assert_eq!(
        out.receipt.extra.get("heap_fidelity"),
        Some(&serde_json::json!("blob_allocations_estimated")),
        "{test}: the receipt must carry the heap fidelity declaration: {:?}",
        out.receipt.extra
    );
    assert_eq!(
        out.receipt.extra.get("heap_ledger"),
        Some(&serde_json::json!({
            "ble": { "blocks": PK_LEDGER_BLOCKS, "bytes": PK_LEDGER_BYTES },
        })),
        "{test}: the receipt must say what the declaration is about: {:?}",
        out.receipt.extra
    );
    assert!(
        out.text.contains("ledger ble.acl_rx")
            && out.text.contains("class=A blob_allocations_estimated")
            && out.text.contains(&format!(
                "ledger total blocks={PK_LEDGER_BLOCKS} bytes={PK_LEDGER_BYTES}"
            )),
        "{test}: the ledger lines are missing from:\n{}",
        out.text
    );
    call("stop", serde_json::json!({"instance": id}));
}

/// The registry and its hooks are process state, so the tests that install them take turns.
static PK_HOST: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Installs the host backend and the introspection walkers (which need the ELF's DWARF) over the
/// corpus `pk` image and ELF, or `None` after the SKIP line.
fn pk_host(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    let bin = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let elf = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport.elf")?;
    let guard = PK_HOST.lock().unwrap_or_else(|e| e.into_inner());
    static WORLD: std::sync::OnceLock<(Arc<Vec<u8>>, Arc<pemu_host::hooks::ElfContext>)> =
        std::sync::OnceLock::new();
    let (image, context) = WORLD.get_or_init(|| {
        let elf = std::fs::read(elf).expect("the verified corpus ELF is readable");
        (
            Arc::new(std::fs::read(bin).expect("the verified corpus image is readable")),
            Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses")),
        )
    });
    let (image, context) = (Arc::clone(image), Arc::clone(context));
    let scratch = std::env::temp_dir().join(format!("pemu-m8-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(scratch.join("audio")).expect("a scratch audio root");
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            common::PK => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(scratch.join("audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == common::PK).then(|| Arc::clone(&context))),
        // The workspace is the scenario root, so `tests/scenarios/ble-gatt.yaml` reads by its path.
        scenario_root: pemu_host::hooks::ScenarioRoot::new(Some(workspace()), vec![workspace()]),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    Some(guard)
}

/// [`pk_host`] for the GATT probe `pkgatt`, under the same lock.
fn pkgatt_host(test: &str) -> Option<std::sync::MutexGuard<'static, ()>> {
    let bin = common::corpus_file_or_skip(test, "pkgatt", "merged-binary.bin")?;
    let elf = common::corpus_file_or_skip(test, "pkgatt", "radio_pkgatt.elf")?;
    let guard = PK_HOST.lock().unwrap_or_else(|e| e.into_inner());
    static WORLD: std::sync::OnceLock<(Arc<Vec<u8>>, Arc<pemu_host::hooks::ElfContext>)> =
        std::sync::OnceLock::new();
    let (image, context) = WORLD.get_or_init(|| {
        let elf = std::fs::read(elf).expect("the verified corpus ELF is readable");
        (
            Arc::new(std::fs::read(bin).expect("the verified corpus image is readable")),
            Arc::new(pemu_host::hooks::ElfContext::parse(&elf).expect("the ELF parses")),
        )
    });
    let (image, context) = (Arc::clone(image), Arc::clone(context));
    let scratch = std::env::temp_dir().join(format!("pemu-m8-{test}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(scratch.join("audio")).expect("a scratch audio root");
    pemu_host::backend::install(
        Arc::new(move |fw: &str| match fw {
            "pkgatt" => pemu_host::backend::merged_image(&image),
            other => Err(pemu_api::commands::start::firmware_not_found(other)),
        }),
        None,
        pemu_host::audio_root::AudioRoot::new(scratch.join("audio")),
    );
    pemu_host::hooks::install(pemu_host::hooks::HostHooks {
        elves: Arc::new(move |fw: &str| (fw == "pkgatt").then(|| Arc::clone(&context))),
        scenario_root: pemu_host::hooks::ScenarioRoot::new(None, Vec::new()),
        salt_dir: None,
    });
    pemu_host::boot_cache::install(None);
    Some(guard)
}

/// One registry call, as the daemon and the CLI make it, which must succeed.
#[track_caller]
fn call(name: &str, args: serde_json::Value) -> pemu_api::output::Output {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone())
        .unwrap_or_else(|e| panic!("`{name}` {args} failed: {e:?}"))
}

/// A host-posted radio event is a journaled input (`Origin::Bridge`, so the run is `live`), and
/// its replay reaches the same state hash.
#[test]
fn t1_m8_a_host_posted_radio_event_is_journaled_and_replays_to_the_same_state() {
    use pemu_core::journal::Determinism;
    let test = "t1_m8_a_host_posted_radio_event_is_journaled_and_replays_to_the_same_state";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut m = machine(&flash, &elf, MachineConfig::default());
    run_to_line(&mut m, "pk_app: ready", 5_000, test);
    assert_eq!(
        m.journal().class(),
        Determinism::Deterministic,
        "{test}: nothing live yet"
    );
    let before = ble_state(&m).rx_packets;
    let seq = m.journal().live_next(LiveStream::Hci);
    journal_event(&mut m, seq);
    let end = VTime::from_us(m.now().as_us() + 500_000);
    let limits = || RunLimits {
        until: Some(end),
        max_insns: None,
        stops: StopSet::default(),
    };
    let out = m.run(limits());
    assert_eq!(out.reason, StopReason::Until, "{test}");
    // Not vacuous: the packet reached the guest's VHCI callback.
    assert!(
        ble_state(&m).rx_packets > before,
        "{test}: the journaled packet was delivered"
    );
    assert_eq!(
        ble_state(&m).external.inbound,
        1,
        "{test}: the bridge counted it"
    );
    assert_eq!(
        m.journal().class(),
        Determinism::Live,
        "{test}: a bridged peer makes the run live"
    );
    let want = m.state_hash();

    let want_console = console(&mut m);
    let want_state = ble_state(&m);

    // The replay keeps the recorded origins: the journal's class is hashed state, so dropping them
    // would reach the same guest and a different hash.
    let mut replay = machine(&flash, &elf, MachineConfig::default());
    for entry in m.journal().entries() {
        replay
            .input_from(
                pemu_machine::machine::At::Vt(entry.at),
                entry.origin,
                entry.ev.clone(),
            )
            .expect("a recorded input journals again");
    }
    let out = replay.run(limits());
    assert_eq!(out.reason, StopReason::Until, "{test}: replay");
    assert_eq!(replay.state_hash(), want, "{test}: replay state differs");
    assert_eq!(
        ble_state(&replay),
        want_state,
        "{test}: replay module state"
    );

    // Replayed through `input`, which carries no origin, the journal reaches the same console and
    // module state; the `state_hash` differs only in the journal's own class.
    let mut exported = machine(&flash, &elf, MachineConfig::default());
    for entry in m.journal().entries() {
        exported
            .input(pemu_machine::machine::At::Vt(entry.at), entry.ev.clone())
            .expect("a recorded input journals again");
    }
    let out = exported.run(limits());
    assert_eq!(out.reason, StopReason::Until, "{test}: export replay");
    assert_eq!(
        exported.journal().class(),
        Determinism::Deterministic,
        "{test}: a replay is deterministic again"
    );
    assert_eq!(
        console(&mut exported),
        want_console,
        "{test}: export console"
    );
    assert_eq!(
        ble_state(&exported),
        want_state,
        "{test}: export module state"
    );
}

/// With no bound BLE module an external HCI packet and a bridge attach are counted unapplied; so
/// is a packet no H4 framing could hold.
#[test]
fn t1_m8_an_external_hci_input_without_a_bound_module_or_out_of_shape_is_unapplied() {
    use pemu_core::input::{EnvChange, InputEvent};
    use pemu_machine::machine::At;
    let test = "t1_m8_an_external_hci_input_without_a_bound_module_or_out_of_shape_is_unapplied";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let step = |m: &mut Machine| {
        m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 1)),
            max_insns: None,
            stops: StopSet::default(),
        });
    };
    let mut m = machine(&flash, &elf, ble_disabled());
    let before = m.unapplied_inputs();
    m.input(
        At::Now,
        InputEvent::HciPacket {
            seq: 0,
            data: RESET_COMPLETE.to_vec(),
        },
    )
    .expect("journals");
    m.input(
        At::Now,
        InputEvent::Env(EnvChange::BleHciBridge { attached: true }),
    )
    .expect("journals");
    step(&mut m);
    assert_eq!(m.unapplied_inputs(), before + 2, "{test}: no ble module");
    assert_eq!(m.radio_module_state("ble"), None, "{test}");

    let mut m = machine(&flash, &elf, MachineConfig::default());
    run_to_line(&mut m, "pk_app: ready", 5_000, test);
    let before = m.unapplied_inputs();
    let state = ble_state(&m);
    m.input(
        At::Now,
        InputEvent::HciPacket {
            seq: 0,
            data: Vec::new(),
        },
    )
    .expect("journals");
    step(&mut m);
    assert_eq!(m.unapplied_inputs(), before + 1, "{test}: empty packet");
    assert_eq!(ble_state(&m).external, state.external, "{test}");
}

// ---------------------------------------------------------------------------------------------
// An external HCI peer under a lease, with `Wall{1}` pacing
// ---------------------------------------------------------------------------------------------

use pemu_radio::ble::controller::Controller;

/// The host-time round trip the external peer takes per packet: a loopback peer with a Python
/// host is this slow, and two runs of the same script varied by 80 ms, so 300 ms is late, not
/// broken.
const PEER_DELAY_MS: u64 = 300;

/// NimBLE's HCI command timeout: it resets the host with reason 19 when a command goes
/// unacknowledged this long.
const NIMBLE_CMD_TIMEOUT_MS: u64 = 2_000;

/// Runs `m` for `until_ms` in 1 ms slices as an external HCI controller answering each bridged
/// packet `delay_ms` later. Packets are read by cursor, never drained, so reading changes no guest
/// state; answers go back as journaled `Origin::Bridge` inputs. Returns packets out and back.
fn drive_external_peer(m: &mut Machine, delay_ms: u64, until_ms: u64, test: &str) -> (u64, u64) {
    use pemu_core::rng::{DetRng, RngStream};
    let mut peer = Controller::default();
    // The `02:00:00` placeholder prefix of docs/secrets.md.
    let address = [0x01, 0x00, 0x00, 0x00, 0x00, 0x02];
    let mut rng = DetRng::new(0x26_04);
    let mut stream = rng.stream(RngStream::RADIO_BLE);
    let mut cursor = 0u64;
    let mut seq = 0u64;
    let mut answered = 0u64;
    while m.now() < VTime::from_ms(until_ms) {
        let out = m.run(RunLimits {
            until: Some(VTime::from_us(m.now().as_us() + 1_000)),
            max_insns: None,
            stops: StopSet::default(),
        });
        assert_eq!(out.reason, StopReason::Until, "{test}: {}", tail(m));
        let st = ble_state(m);
        if !st.external.attached {
            continue;
        }
        let (packets, next) = st.external.since(cursor);
        cursor = next;
        for packet in packets {
            for event in peer.on_packet(address, &packet, &mut |b: &mut [u8]| stream.fill_bytes(b))
            {
                let at = VTime::from_us(m.now().as_us() + delay_ms * 1_000);
                m.input_from(
                    pemu_machine::machine::At::Vt(at),
                    Origin::Bridge,
                    InputEvent::HciPacket { seq, data: event },
                )
                .expect("a bridged answer journals");
                seq += 1;
                answered += 1;
            }
        }
    }
    (cursor, answered)
}

fn attach_bridge(m: &mut Machine, attached: bool) {
    use pemu_core::input::EnvChange;
    m.input_from(
        pemu_machine::machine::At::Now,
        Origin::Bridge,
        InputEvent::Env(EnvChange::BleHciBridge { attached }),
    )
    .expect("the bridge attach journals");
    apply(m);
}

/// With the bridge up from boot, `pk`'s NimBLE startup is answered by a 300 ms peer and the host
/// is never reset. A peer slower than the 2 s command timeout does reset, which is what `Max`
/// pacing would do to a 300 ms peer; hence `Wall{1}` while a bridge is live.
#[test]
fn t1_m8_an_external_hci_peer_with_a_delayed_response_completes_without_a_timeout() {
    let test = "t1_m8_an_external_hci_peer_with_a_delayed_response_completes_without_a_timeout";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut m = machine(&flash, &elf, MachineConfig::default());
    attach_bridge(&mut m, true);
    let (sent, answered) = drive_external_peer(&mut m, PEER_DELAY_MS, 14_000, test);
    let text = console(&mut m);
    assert!(
        !text.contains("host reset"),
        "{test}: a {PEER_DELAY_MS} ms peer must not time the host out:\n{}",
        tail(&mut m)
    );
    // Not vacuous: the guest talked to the external peer, not the virtual controller.
    assert!(
        sent >= 20 && answered >= 20,
        "{test}: the bridge carried {sent} out and {answered} back"
    );
    let st = ble_state(&m);
    assert!(st.external.attached, "{test}");
    assert_eq!(st.external.dropped_out, 0, "{test}: the window held");
    assert_eq!(st.external.lost_in, 0, "{test}: no gap in the stream");
    assert_eq!(
        st.external.inbound, answered,
        "{test}: every answer was applied"
    );
    assert_eq!(
        st.outbox.len(),
        0,
        "{test}: the virtual controller stayed out"
    );
    assert_eq!(
        m.live_bridges(),
        1,
        "{test}: the machine reports its live bridge"
    );

    // A slower peer does reset the host, so the first leg measures something.
    let mut slow = machine(&flash, &elf, MachineConfig::default());
    attach_bridge(&mut slow, true);
    drive_external_peer(&mut slow, NIMBLE_CMD_TIMEOUT_MS + 500, 14_000, test);
    assert!(
        console(&mut slow).contains("host reset"),
        "{test}: a peer past the {NIMBLE_CMD_TIMEOUT_MS} ms timeout must reset the host:\n{}",
        tail(&mut slow)
    );
}

/// While the bridge is live the pacing is `Wall { rate: 1 }`: `clock set_speed max`, `clock
/// set_mode deterministic` and `clock pause` answer `E_LEASE` naming the bridge, `clock status`
/// reports realtime at 1.000x, and detaching gives the clock back.
#[test]
fn t1_m8_a_live_bridge_forces_wall_1x_pacing_and_refuses_every_other_clock() {
    let test = "t1_m8_a_live_bridge_forces_wall_1x_pacing_and_refuses_every_other_clock";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": common::PK}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    call(
        "clock",
        serde_json::json!({"instance": id, "op": "set_speed", "speed": "max"}),
    );
    let before = call("clock", serde_json::json!({"instance": id, "op": "status"}));
    assert_eq!(before.json["speed"], "max", "{test}: {}", before.text);
    assert_eq!(before.json["live_bridges"], 0, "{test}");

    with_session(&id, |session| {
        session
            .machine()
            .input_from(
                pemu_machine::machine::At::Now,
                Origin::Bridge,
                InputEvent::Env(pemu_core::input::EnvChange::BleHciBridge { attached: true }),
            )
            .expect("the bridge attach journals");
    });
    // The input applies at the next slice boundary, which a zero-length run reaches.
    call(
        "run",
        serde_json::json!({"instance": id, "for": "1ms", "timeout": "5s"}),
    );

    let status = call("clock", serde_json::json!({"instance": id, "op": "status"}));
    assert_eq!(status.json["live_bridges"], 1, "{test}: {}", status.text);
    assert_eq!(status.json["mode"], "realtime", "{test}: {}", status.text);
    assert_eq!(status.json["speed"], 1.0, "{test}: {}", status.text);
    assert!(
        status.text.contains("speed=1.000x"),
        "{test}: {}",
        status.text
    );

    for args in [
        serde_json::json!({"instance": id, "op": "set_speed", "speed": "max"}),
        serde_json::json!({"instance": id, "op": "set_mode", "mode": "deterministic"}),
        serde_json::json!({"instance": id, "op": "pause"}),
    ] {
        let err = call_err("clock", args.clone());
        assert_eq!(err.code.name, "E_LEASE", "{test}: {args} gave {err:?}");
        assert!(
            err.message.contains("bridge"),
            "{test}: {args} must name the bridge: {}",
            err.message
        );
    }
    // `endpoint --clock agent` takes the clock too, and is refused the same way before the host
    // opens anything.
    let agent_clock = call_err(
        "endpoint",
        serde_json::json!({"instance": id, "tcp": true, "clock": "agent"}),
    );
    assert_eq!(agent_clock.code.name, "E_LEASE", "{test}: {agent_clock:?}");
    assert!(
        agent_clock.message.contains("bridge"),
        "{test}: the refusal names the bridge: {}",
        agent_clock.message
    );

    call(
        "clock",
        serde_json::json!({"instance": id, "op": "set_speed", "speed": 1.0}),
    );

    with_session(&id, |session| {
        session
            .machine()
            .input_from(
                pemu_machine::machine::At::Now,
                Origin::Bridge,
                InputEvent::Env(pemu_core::input::EnvChange::BleHciBridge { attached: false }),
            )
            .expect("the bridge detach journals");
    });
    call(
        "run",
        serde_json::json!({"instance": id, "for": "1ms", "timeout": "5s"}),
    );
    let after = call("clock", serde_json::json!({"instance": id, "op": "status"}));
    assert_eq!(after.json["live_bridges"], 0, "{test}: {}", after.text);
    call(
        "clock",
        serde_json::json!({"instance": id, "op": "set_speed", "speed": "max"}),
    );
    call("stop", serde_json::json!({"instance": id}));
}

// ---------------------------------------------------------------------------------------------
// The same exchange, driven through the three registry commands
// ---------------------------------------------------------------------------------------------

use pemu_loader::hex as hex_of;

/// The whole exchange through `ble_scan`, `ble_connect` and `ble_gatt` on a real `pk` session:
/// the scan reports the advertiser by address and name, the connect exchanges MTU 247, `discover`
/// returns the vendor service as a tree, the lines go through `write` and `notifications`, and
/// the `capture` parses.
#[test]
fn t1_m8_pk_gatt_hello_and_ping_through_the_ble_commands() {
    let test = "t1_m8_pk_gatt_hello_and_ping_through_the_ble_commands";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": common::PK}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/pk_app: ready/", "timeout": "10s"}),
    );

    let scan = call(
        "ble_scan",
        serde_json::json!({"instance": id, "duration_ms": 500}),
    );
    let found = scan.json["found"].as_array().expect("a list").clone();
    let advertiser = found
        .iter()
        .find(|r| r["connectable"] == true)
        .unwrap_or_else(|| panic!("{test}: nothing connectable on the air: {}", scan.text));
    assert_eq!(advertiser["pdu"], "ADV_IND", "{test}: {}", scan.text);
    // The address is the placeholder-OUI BD_ADDR the eFuse base MAC gives it, not a device's.
    assert_eq!(advertiser["name"], "Passport Keys", "{test}: {}", scan.text);
    assert!(
        advertiser["addr"]
            .as_str()
            .is_some_and(|a| a.starts_with("02:00:00")),
        "{test}: {}",
        scan.text
    );
    let addr = advertiser["addr"].as_str().expect("an address").to_owned();

    let connected = call(
        "ble_connect",
        serde_json::json!({"instance": id, "addr": addr}),
    );
    assert_eq!(
        connected.json["connected"], true,
        "{test}: {}",
        connected.text
    );
    assert_eq!(connected.json["mtu"], 247, "{test}: {}", connected.text);

    let tree = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "discover"}),
    );
    let services = tree.json["tree"].as_array().expect("a tree").clone();
    let vendor = services
        .iter()
        .find(|s| s["uuid"] == PK_SERVICE)
        .unwrap_or_else(|| panic!("{test}: no vendor service discovered: {}", tree.text));
    let characteristics = vendor["children"].as_array().expect("children");
    for uuid in [PK_EVENTS, PK_COMMANDS] {
        assert!(
            characteristics.iter().any(|c| c["uuid"] == uuid),
            "{test}: {uuid} is not under the vendor service: {}",
            tree.text
        );
    }

    let subscribed = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "subscribe", "uuid": PK_EVENTS}),
    );
    assert!(
        subscribed.json["cccd"].is_number(),
        "{test}: the subscribe named no CCCD: {}",
        subscribed.text
    );
    for (line, answer) in [
        ("{\"cmd\":\"hello\"}\n", "\"t\":\"hello\""),
        ("{\"cmd\":\"ping\"}\n", "{\"t\":\"pong\"}"),
    ] {
        call(
            "ble_gatt",
            serde_json::json!({
                "instance": id, "op": "write", "uuid": PK_COMMANDS, "text": line
            }),
        );
        let got = call(
            "ble_gatt",
            serde_json::json!({
                "instance": id, "op": "notifications", "uuid": PK_EVENTS,
                "contains": hex_of(answer.as_bytes()), "within_ms": 5_000
            }),
        );
        assert_eq!(got.json["status"], "ok", "{test}: {line}: {}", got.text);
        let text: String = got.json["notifications"]
            .as_array()
            .expect("a list")
            .iter()
            .filter_map(|n| n["text"].as_str())
            .collect();
        assert!(
            text.contains(answer),
            "{test}: {line} was not answered with {answer}: {text}"
        );
    }

    let console = call(
        "serial",
        serde_json::json!({"instance": id, "op": "read", "cursor": 0}),
    );
    assert!(
        !console.text.contains("host reset"),
        "{test}: the host reset during the exchange:\n{}",
        console.text
    );
    let capture = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "capture", "artifact": false}),
    );
    assert_eq!(capture.json["datalink"], 1002, "{test}: {}", capture.text);
    assert!(
        capture.json["packets"].as_u64().unwrap_or(0) > 0,
        "{test}: the capture is empty: {}",
        capture.text
    );
    let file = with_session(&id, |session| {
        let bytes = session
            .machine()
            .radio_module_state("ble")
            .expect("the ble module is bound")
            .to_vec();
        BleState::decode(&bytes)
            .expect("the module state decodes")
            .capture
            .to_btsnoop()
    });
    let parsed = btsnoop::parse(&file).unwrap_or_else(|e| panic!("{test}: btsnoop: {e}"));
    assert_eq!(
        parsed.len(),
        capture.json["packets"].as_u64().unwrap_or(0) as usize,
        "{test}: the command counted a different number of packets than the file holds"
    );

    call(
        "ble_connect",
        serde_json::json!({"instance": id, "disconnect": true}),
    );
    call("stop", serde_json::json!({"instance": id}));
}

const BLE_GATT_SCENARIO: &str = "tests/scenarios/ble-gatt.yaml";

/// `tests/scenarios/ble-gatt.yaml` runs through the scenario runner on a real `pk` session and
/// every step passes; `expect_not` with `from: start` keeps `host reset` out of the whole console.
///
/// A scenario cannot see the capture's bytes, so this test parses the btsnoop itself, checks the
/// `from: start` window, and shows the assertions are not vacuous (datalink 1001 fails).
#[test]
fn t1_m8_pk_gatt_hello_and_ping_through_the_ble_gatt_scenario() {
    let test = "t1_m8_pk_gatt_hello_and_ping_through_the_ble_gatt_scenario";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": common::PK}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/pk_app: ready/", "timeout": "10s"}),
    );

    let out = call(
        "scenario",
        serde_json::json!({"file": BLE_GATT_SCENARIO, "instance": id, "strict": false}),
    );
    let report = &out.json["scenarios"][0];
    // A caveat picked up on the way is a fact about the machine, not a failed step.
    let status = report["status"].as_str().unwrap_or_default();
    assert!(
        status == "pass" || status == "pass_with_caveats",
        "{test}: {status}\n{}\n{}",
        out.text,
        out.json
    );
    assert_eq!(report["source"], BLE_GATT_SCENARIO, "{test}: {}", out.json);
    let steps: Vec<(String, String)> = report["steps"]
        .as_array()
        .expect("step reports")
        .iter()
        .map(|s| {
            (
                s["key"].as_str().unwrap_or_default().to_owned(),
                s["status"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    let keys: Vec<&str> = steps.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "ble.scan",
            "ble.connect",
            "ble.gatt",
            "ble.gatt",
            "ble.gatt",
            "ble.gatt",
            "ble.gatt",
            "ble.gatt",
            "expect_not",
            "ble.gatt",
        ],
        "{test}: the file's steps, in order: {}",
        out.json
    );
    assert!(
        steps.iter().all(|(_, s)| s == "pass"),
        "{test}: every step passes: {steps:?}\n{}",
        out.json
    );

    let notified = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "notifications", "uuid": PK_EVENTS}),
    );
    let text: String = notified.json["notifications"]
        .as_array()
        .expect("a list")
        .iter()
        .filter_map(|n| n["text"].as_str())
        .collect();
    for answer in ["\"t\":\"hello\"", "{\"t\":\"pong\"}"] {
        assert!(
            text.contains(answer),
            "{test}: {answer} was not notified: {text}"
        );
    }
    let console = call(
        "serial",
        serde_json::json!({"instance": id, "op": "read", "cursor": 0, "max_bytes": 200_000}),
    );
    assert!(
        console.text.contains("pk_app: ready") && !console.text.contains("host reset"),
        "{test}: the console holds the boot and no host reset:\n{}",
        console.text
    );
    let capture = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "capture", "artifact": false}),
    );
    let packets = capture.json["packets"].as_u64().unwrap_or(0);
    assert!(
        packets > 0,
        "{test}: the capture is empty: {}",
        capture.text
    );
    let file = with_session(&id, |session| {
        let bytes = session
            .machine()
            .radio_module_state("ble")
            .expect("the ble module is bound")
            .to_vec();
        BleState::decode(&bytes)
            .expect("the module state decodes")
            .capture
            .to_btsnoop()
    });
    let parsed = btsnoop::parse(&file).unwrap_or_else(|e| panic!("{test}: btsnoop: {e}"));
    assert_eq!(
        parsed.len(),
        packets as usize,
        "{test}: the capture step counted a different number of packets than the file holds"
    );

    // Not vacuous: the same kind of step with an answer field it does not hold fails.
    let wrong = call(
        "scenario",
        serde_json::json!({
            "inline": "schema: passportsim/scenario@1\nname: wrong\nsteps:\n  - ble.gatt: {op: capture, artifact: false}\n    expect: {datalink: 1001}\n",
            "instance": id,
        }),
    );
    let wrong_step = &wrong.json["scenarios"][0]["steps"][0];
    assert_eq!(wrong_step["status"], "fail", "{test}: {}", wrong.json);
    assert_eq!(
        wrong_step["error"]["code"], "E_ASSERT",
        "{test}: {}",
        wrong.json
    );
    assert!(
        wrong_step["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("`datalink`: expected 1001, saw 1002")),
        "{test}: {}",
        wrong.json
    );
    // The file's `host reset` window reaches back to the boot.
    let boot_line = call(
        "scenario",
        serde_json::json!({
            "inline": "schema: passportsim/scenario@1\nname: boot-line\nsteps:\n  - expect_not: {serial: {re: \"pk_app: ready\", from: start}, for: 1ms}\n",
            "instance": id,
        }),
    );
    assert_eq!(
        boot_line.json["scenarios"][0]["steps"][0]["status"], "fail",
        "{test}: `from: start` sees the whole console: {}",
        boot_line.json
    );

    call(
        "ble_connect",
        serde_json::json!({"instance": id, "disconnect": true}),
    );
    call("stop", serde_json::json!({"instance": id}));
    println!(
        "RAN {test}: {BLE_GATT_SCENARIO} {status}, {} steps pass, {packets} btsnoop packets parse",
        steps.len()
    );
}

/// The same exchange on `pkgatt` through the three registry commands gives the console lines of
/// the reference run, without a script the test journals.
#[test]
fn t1_m8_pkgatt_console_matches_g2_gt3_through_the_ble_commands() {
    let test = "t1_m8_pkgatt_console_matches_g2_gt3_through_the_ble_commands";
    let Some(host) = pkgatt_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": "pkgatt", "boot": "none"}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/RC\\|pk_ble_start\\|0/", "timeout": "10s"}),
    );
    call(
        "ble_scan",
        serde_json::json!({"instance": id, "duration_ms": 500}),
    );
    let connected = call(
        "ble_connect",
        serde_json::json!({"instance": id, "service": "pk"}),
    );
    assert_eq!(
        connected.json["connected"], true,
        "{test}: {}",
        connected.text
    );
    call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "discover"}),
    );
    call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "subscribe", "uuid": PK_EVENTS}),
    );
    for (line, answer) in [
        ("{\"cmd\":\"hello\"}\n", "\"t\":\"hello\""),
        ("{\"cmd\":\"ping\"}\n", "{\"t\":\"pong\"}"),
    ] {
        call(
            "ble_gatt",
            serde_json::json!({
                "instance": id, "op": "write", "uuid": PK_COMMANDS, "text": line
            }),
        );
        let got = call(
            "ble_gatt",
            serde_json::json!({
                "instance": id, "op": "notifications", "uuid": PK_EVENTS,
                "contains": hex_of(answer.as_bytes()), "within_ms": 5_000
            }),
        );
        assert_eq!(got.json["status"], "ok", "{test}: {line}: {}", got.text);
    }
    call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/PROBE DONE/", "timeout": "20s"}),
    );
    let console = call(
        "serial",
        serde_json::json!({"instance": id, "op": "read", "cursor": 0, "max_bytes": 200_000}),
    );
    let text = console.json["text"]
        .as_str()
        .unwrap_or(&console.text)
        .to_owned();
    // The reference run's lines with their `|t_ms=` suffix removed.
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.split("|t_ms=").next().unwrap_or(l))
        .map(str::trim)
        .filter(|l| l.starts_with("EVT|LINK") || l.starts_with("RX|") || l.starts_with("RC|gatt"))
        .collect();
    assert_eq!(
        lines,
        [
            "EVT|LINK|subscribed=1",
            "RX|1|{\"cmd\":\"hello\"}",
            "RX|2|{\"cmd\":\"ping\"}",
            "RC|gatt|subscribed=1|lines=2|tx=3",
        ],
        "{test}: {text}"
    );
    call("stop", serde_json::json!({"instance": id}));
}

/// The full-fidelity btsnoop capture is an opt-in that taints: it is journaled, it is module
/// state so it survives the call that set it, every later receipt says `tainted`, and
/// `snapshot export` refuses without the confirmation. The packet level is covered by
/// `pemu_radio::ble::btsnoop`'s unit tests.
#[test]
fn t1_m8_a_full_fidelity_capture_taints_the_instance_and_its_export_needs_the_confirmation() {
    let test =
        "t1_m8_a_full_fidelity_capture_taints_the_instance_and_its_export_needs_the_confirmation";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    let started = call("start", serde_json::json!({"fw": common::PK}));
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();
    assert!(
        !started.receipt.tainted,
        "{test}: a `pk` image is not a secret-bearing input: {}",
        started.text
    );
    call(
        "run",
        serde_json::json!({"instance": id, "until": "serial:/pk_app: ready/", "timeout": "10s"}),
    );

    let plain = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "capture", "artifact": false}),
    );
    assert_eq!(plain.json["secrets"], false, "{test}: {}", plain.text);
    assert!(!plain.receipt.tainted, "{test}: {}", plain.text);

    let opted = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "capture", "secrets": true, "artifact": false}),
    );
    assert_eq!(opted.json["secrets"], true, "{test}: {}", opted.text);
    assert!(
        opted.receipt.tainted,
        "{test}: the call that asked for it is already tainted: {}",
        opted.text
    );
    assert!(
        opted.text.contains("key material kept"),
        "{test}: the text says what the file holds: {}",
        opted.text
    );

    // Module state: the next call sees it untold. This host has no artifacts root, so only the mode
    // and the taint are asserted here.
    let after = call(
        "ble_gatt",
        serde_json::json!({"instance": id, "op": "capture", "artifact": false}),
    );
    assert_eq!(after.json["secrets"], true, "{test}: {}", after.text);
    assert!(after.receipt.tainted, "{test}: {}", after.text);

    let refused = call_err(
        "snapshot",
        serde_json::json!({"instance": id, "op": "export", "name": "ble-secrets"}),
    );
    assert_eq!(
        refused.code,
        pemu_api::error::E_SECRET_REFUSED,
        "{test}: {refused:?}"
    );
    let still_refused = call_err(
        "snapshot",
        serde_json::json!({"instance": id, "op": "export", "name": "ble-secrets", "include_secrets": true}),
    );
    assert_eq!(
        still_refused.code,
        pemu_api::error::E_SECRET_REFUSED,
        "{test}: `include_secrets` alone is not the confirmation: {still_refused:?}"
    );
    // With the confirmation the gate lets it through, and the call fails at the writer instead
    // (no artifacts root).
    let past_the_gate = call_err(
        "snapshot",
        serde_json::json!({"instance": id, "op": "export", "name": "ble-secrets", "include_secrets": true, "confirm": "yes"}),
    );
    assert_eq!(
        past_the_gate.code,
        pemu_api::error::E_STATE,
        "{test}: the confirmed export is no longer refused as secret-bearing: {past_the_gate:?}"
    );
    assert!(
        past_the_gate
            .message
            .contains("artifact could not be written"),
        "{test}: {past_the_gate:?}"
    );
    call("stop", serde_json::json!({"instance": id}));
}

/// The transport itself: `env --ble-bridge attach` opens the loopback listener, and a
/// `Controller` on the other end of the socket answers `pk`'s NimBLE startup, adding the H4
/// framing, the socket and the pump to the machine-level test.
#[test]
fn t1_m8_the_external_hci_transport_carries_the_startup_over_a_loopback_socket() {
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let test = "t1_m8_the_external_hci_transport_carries_the_startup_over_a_loopback_socket";
    let Some(host) = pk_host(test) else {
        return;
    };
    let _ = &host;
    // Booted only to `app_main`: under U4 NimBLE's startup is answered before the default boot ends
    // (advertising starts at about 415 ms of 440).
    let started = call(
        "start",
        serde_json::json!({"fw": common::PK, "boot": "until_app_main"}),
    );
    let id = started.json["instance"]
        .as_str()
        .expect("an instance id")
        .to_owned();

    // The bridge goes up before the guest's stack starts, so the startup crosses the socket.
    let attached = call(
        "env",
        serde_json::json!({"instance": id, "ble_bridge": "attach"}),
    );
    let url = attached.json["applied"]["ble_bridge"]["url"]
        .as_str()
        .unwrap_or_else(|| panic!("{test}: no transport was opened: {}", attached.text))
        .to_owned();
    assert!(
        url.starts_with("hci://127.0.0.1:"),
        "{test}: a bridge listens on loopback only: {url}"
    );
    let port: u16 = url
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or_else(|| panic!("{test}: no port in {url}"));

    let stop = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(AtomicU64::new(0));
    let taken = Arc::new(AtomicU64::new(0));
    let peer = std::thread::spawn({
        let stop = Arc::clone(&stop);
        let answered = Arc::clone(&answered);
        let taken = Arc::clone(&taken);
        move || {
            use pemu_core::rng::{DetRng, RngStream};
            let mut socket = std::net::TcpStream::connect(("127.0.0.1", port))
                .expect("the bridge listens on loopback");
            socket
                .set_read_timeout(Some(std::time::Duration::from_millis(20)))
                .expect("a read timeout");
            socket
                .set_write_timeout(Some(std::time::Duration::from_millis(500)))
                .expect("a write timeout");
            let mut controller = Controller::default();
            let address = [0x01, 0x00, 0x00, 0x00, 0x00, 0x02];
            let mut rng = DetRng::new(0x26_05);
            let mut stream = rng.stream(RngStream::RADIO_BLE);
            let mut framer = pemu_api::commands::ble_scan::H4Stream::new();
            let mut buf = [0u8; 4096];
            while !stop.load(Ordering::SeqCst) {
                let read = match socket.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => continue,
                };
                for packet in framer.feed(&buf[..read]).expect("the stream stays framed") {
                    taken.fetch_add(1, Ordering::SeqCst);
                    for event in controller
                        .on_packet(address, &packet, &mut |b: &mut [u8]| stream.fill_bytes(b))
                    {
                        if socket.write_all(&event).is_err() {
                            return;
                        }
                        answered.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        }
    });

    // A command checks the session out for as long as it runs and a live bridge holds the pacing at
    // wall 1x, so the gap between slices is the transport's window to journal into.
    let mut inbound = 0u64;
    for round in 0..120 {
        call(
            "run",
            serde_json::json!({"instance": id, "for": "20ms", "timeout": "5s"}),
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        if round % 5 == 0 {
            inbound = with_session(&id, |session| {
                pemu_api::commands::ble_scan::ble_state(session)
                    .expect("the module state decodes")
                    .external
                    .inbound
            });
            if taken.load(Ordering::SeqCst) >= 20 && inbound >= 20 {
                break;
            }
        }
    }
    stop.store(true, Ordering::SeqCst);

    let state = with_session(&id, |session| {
        pemu_api::commands::ble_scan::ble_state(session).expect("the module state decodes")
    });
    println!(
        "{test}: taken={} answered={} inbound={} sent={} lost_in={} dropped_out={}",
        taken.load(Ordering::SeqCst),
        answered.load(Ordering::SeqCst),
        inbound,
        state.external.sent,
        state.external.lost_in,
        state.external.dropped_out
    );
    assert!(state.external.attached, "{test}: the bridge stayed up");
    assert!(
        taken.load(Ordering::SeqCst) >= 20,
        "{test}: the peer took only {} packet(s) off the socket",
        taken.load(Ordering::SeqCst)
    );
    assert!(
        state.external.inbound >= 20,
        "{test}: only {} answer(s) were journaled",
        state.external.inbound
    );
    assert_eq!(state.external.lost_in, 0, "{test}: no gap in the stream");
    assert_eq!(state.external.dropped_out, 0, "{test}: the window held");
    assert_eq!(
        state.outbox.len(),
        0,
        "{test}: the virtual controller stayed out while the bridge held the boundary"
    );

    let detached = call(
        "env",
        serde_json::json!({"instance": id, "ble_bridge": "detach"}),
    );
    assert_eq!(
        detached.json["applied"]["ble_bridge"]["state"], "detached",
        "{test}: {}",
        detached.text
    );
    call(
        "run",
        serde_json::json!({"instance": id, "for": "1ms", "timeout": "5s"}),
    );
    let after = with_session(&id, |session| {
        pemu_api::commands::ble_scan::ble_state(session).expect("the module state decodes")
    });
    assert!(!after.external.attached, "{test}: the bridge came down");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "{test}: the listener is closed with the bridge"
    );
    let _ = peer.join();
    call("stop", serde_json::json!({"instance": id}));
}

#[track_caller]
fn call_err(name: &str, args: serde_json::Value) -> pemu_api::error::ApiError {
    let spec = pemu_api::registry::find(name).unwrap_or_else(|| panic!("`{name}` is registered"));
    match (spec.handler)(&mut pemu_api::spec::HandlerCx {}, args.clone()) {
        Err(e) => e,
        Ok(out) => panic!("`{name}` {args} was expected to fail, gave: {}", out.text),
    }
}

fn with_session<R>(id: &str, f: impl FnOnce(&mut pemu_api::commands::start::Session) -> R) -> R {
    let parsed = pemu_api::instance::InstanceId::parse(id).expect("an instance id");
    pemu_api::commands::start::with_pool(|pool| f(pool.session_mut(parsed).expect("a session")))
}

// ---------------------------------------------------------------------------------------------
// Snapshots with outstanding continuations, restored in a fresh process
// ---------------------------------------------------------------------------------------------

fn has_continuation(m: &Machine, handler: &str) -> bool {
    m.hle_section()
        .continuations
        .iter()
        .any(|(_, c)| c.handler.handler == handler)
}

const E7_TAIL_US: u64 = 3_000_000;

/// The answer a restored run gives, whichever process ran it: the final `state_hash` and the
/// console after the save.
fn e7_answer(m: &mut Machine, cursor: u64) -> String {
    let hash = pemu_machine::determinism::hex(&m.state_hash());
    let ring = m.io().serial_ring(SerialStream::UsjTx);
    let after: Vec<u8> = ring.slices(cursor).iter().copied().collect();
    format!("{hash}\n---\n{}", String::from_utf8_lossy(&after))
}

fn e7_limits(end_us: u64) -> RunLimits {
    RunLimits {
        until: Some(VTime::from_us(end_us)),
        max_insns: None,
        stops: StopSet::default(),
    }
}

/// Saves taken while an HLE continuation is outstanding (in `esp_bt_controller_init`, inside the
/// GATT exchange, and the U4 in-ISR instant) restore in a fresh machine and a fresh process and
/// continue to the same console and `state_hash` as the uninterrupted run.
///
/// The fresh process matters: `BleProfile::load`, `LogLines::load` and `MagicPcs::from_spec` are
/// process-global, so only another process builds them from the specs again.
#[test]
fn t1_m8_snapshots_with_outstanding_continuations_restore_in_a_fresh_process() {
    let test = "t1_m8_snapshots_with_outstanding_continuations_restore_in_a_fresh_process";
    if let Some(file) = determinism::child_snapshot() {
        e7_child(test, &file);
        return;
    }
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let mut snaps: Vec<(String, Vec<u8>)> = Vec::new();
    let mut wants: Vec<(String, String)> = Vec::new();

    for point in ["init", "gatt", "isr"] {
        let u4 = point == "isr";
        let cfg = if u4 {
            u4_config()
        } else {
            MachineConfig::default()
        };
        let mut m = machine(&flash, &elf, cfg.clone());
        match point {
            // `bsp_batt` is the last device line before BLE init (dev:L67); short steps from there land
            // inside the handler's nested calls.
            "init" => {
                run_to_line(&mut m, "bsp_batt", 5_000, test);
                loop {
                    assert!(
                        m.now() < VTime::from_ms(5_000),
                        "{test} {point}: ble.init never had a continuation outstanding"
                    );
                    if has_continuation(&m, "ble.init") {
                        break;
                    }
                    let out = m.run(RunLimits::insns(32));
                    assert!(
                        matches!(out.reason, StopReason::MaxInsns | StopReason::Until),
                        "{test} {point}: {:?}",
                        out.reason
                    );
                }
            }
            "gatt" => {
                run_to_line(&mut m, "pk_app: ready", 5_000, test);
                run_to_advertising(&mut m, 5_000, test);
                journal_script(&mut m, &gatt_script());
                loop {
                    let st = ble_state(&m);
                    let pending = st
                        .central
                        .link
                        .as_ref()
                        .is_some_and(|l| l.pending.is_some());
                    if st.central.results.len() >= 7 && pending {
                        break;
                    }
                    assert!(
                        m.now() < VTime::from_ms(15_000),
                        "{test} {point}: no request ever in flight"
                    );
                    let out = m.run(RunLimits {
                        until: Some(VTime::from_us(m.now().as_us() + 1_000)),
                        max_insns: None,
                        stops: StopSet::default(),
                    });
                    assert_eq!(out.reason, StopReason::Until, "{test} {point}");
                }
            }
            _ => {
                let out = m.run(one_second());
                assert_eq!(out.reason, StopReason::Until, "{test} {point}");
                let symbols = m.assets().app_elf.clone().expect("elf").symbols.clone();
                let syms = GateSyms {
                    yield_from_isr: symbols.addr_of("vPortYieldFromISR").expect("linked"),
                    host_rcv_pkt: symbols.addr_of("host_rcv_pkt").expect("linked"),
                    enter_critical: symbols.addr_of("vPortEnterCritical").expect("linked"),
                    exit_critical: symbols.addr_of("vPortExitCritical").expect("linked"),
                    current_tcb: symbols.addr_of("pxCurrentTCBs").expect("linked"),
                };
                post(&mut m);
                to_magic_isr_yield(&mut m, &syms, test);
            }
        }
        assert!(
            !m.hle_section().continuations.is_empty(),
            "{test} {point}: the save must have an outstanding continuation"
        );
        let cursor = m.io().serial_ring(SerialStream::UsjTx).head();
        let end_us = m.now().as_us() + E7_TAIL_US;
        let bytes = m
            .snapshot(SnapOpts::default())
            .to_bytes()
            .expect("a machine snapshot serializes");

        let mut restored = machine(&flash, &elf, cfg.clone());
        restored
            .restore(&Snapshot::from_bytes(&bytes).expect("parses"))
            .expect("restores");
        assert_eq!(
            restored.state_hash(),
            m.state_hash(),
            "{test} {point}: the restore differs at the save instant"
        );
        let at_save = m.state_hash();
        let (x, y) = (m.run(e7_limits(end_us)), restored.run(e7_limits(end_us)));
        assert_eq!((x.reason, x.insns), (y.reason, y.insns), "{test} {point}");
        let want = e7_answer(&mut m, cursor);
        assert_eq!(
            e7_answer(&mut restored, cursor),
            want,
            "{test} {point}: the in-process restore diverged"
        );
        // Not vacuous: the guest ran on past the save. `pk` prints nothing during the GATT exchange, so
        // the console is a witness only where it says something.
        assert!(x.insns > 0 && m.state_hash() != at_save, "{test} {point}");
        match point {
            "init" => assert!(
                want.contains("BLE_INIT"),
                "{test} {point}: the save must split the BLE_INIT lines: {want}"
            ),
            "gatt" => {
                assert_exchange(&mut restored, test);
                assert!(
                    ble_state(&restored).central.results.len() > 7,
                    "{test} {point}: the restored central finished no further step"
                );
            }
            _ => assert!(
                ble_state(&restored).rx_packets > 0,
                "{test} {point}: the restored machine delivered the posted event"
            ),
        }
        snaps.push((format!("{point}-{cursor}-{end_us}"), bytes));
        wants.push((point.to_string(), want));
    }

    let answers = determinism::fresh_process(test, &snaps);
    assert_eq!(answers.len(), wants.len());
    for ((point, want), got) in wants.iter().zip(&answers) {
        assert_eq!(
            got, want,
            "{test} {point}: the fresh-process restore differs from the in-process one"
        );
        println!("RAN {test}: {point} restores identically in a fresh process");
    }
}

/// In a child of [`determinism::fresh_process`]: restore the parent's snapshot and answer as the
/// parent's restored machine did.
fn e7_child(test: &str, file: &std::path::Path) {
    let tag = determinism::child_tag(file);
    let mut parts = tag.split('-');
    let point = parts.next().expect("a point name").to_string();
    let cursor: u64 = parts.next().expect("a cursor").parse().expect("a number");
    let end_us: u64 = parts.next().expect("an end").parse().expect("a number");
    let (flash, elf) = pk_files(test).expect("the parent found the corpus");
    let cfg = if point == "isr" {
        u4_config()
    } else {
        MachineConfig::default()
    };
    let mut m = machine(&flash, &elf, cfg);
    let bytes = std::fs::read(file).expect("the snapshot file is readable");
    m.restore(&Snapshot::from_bytes(&bytes).expect("parses"))
        .expect("restores");
    m.run(e7_limits(end_us));
    determinism::child_answer(file, &e7_answer(&mut m, cursor));
}

// ---------------------------------------------------------------------------------------------
// F7 and the BLE demand ledger
// ---------------------------------------------------------------------------------------------

/// Ceiling on the repeats, not a count: `xtask bench` stops once three in a row agree within 10 %.
const F7_REPEAT: &str = "15";

/// F7's 30 s body in 100 ms windows.
const F7_WINDOWS: u64 = 300;

/// F7 (`pk`: BLE advertising, one connection, notify stream for 30 s) and its BLE demand per
/// 100 ms window are recorded, by `xtask bench` with a history file of its own.
///
/// The connection setup is one journaled `EnvChange::BleCentral` script, and the notify stream a
/// two-step script every 200 virtual ms, because `pk` notifies only in answer to a command line.
/// Every quantity is guest-side; F7 has no host-time budget, so load and `contended` are printed,
/// not gated.
#[test]
fn t1_m8_f7_and_its_ble_demand_per_100_ms_window_are_recorded() {
    let test = "t1_m8_f7_and_its_ble_demand_per_100_ms_window_are_recorded";
    let id = test.to_string();
    if pk_files(test).is_none() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("pemu-f7-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temporary directory");
    let history = dir.join("history.json");
    let history_arg = history.to_str().expect("a UTF-8 temporary path");

    let stdout = xtask_bench(&[
        "--workload",
        "F7",
        "--repeat",
        F7_REPEAT,
        "--history",
        history_arg,
        "--json",
    ])
    .unwrap_or_else(|e| panic!("{id}: {e}"));
    let record = one_bench_record(&stdout);
    assert_eq!(record["workload"], "F7", "{id}: {stdout}");

    // 1. The BLE half ran; otherwise the demand below would be `pk` idling.
    let ble = &record["ble"];
    let count = |name: &str| -> u64 {
        ble[name]
            .as_u64()
            .unwrap_or_else(|| panic!("{id}: F7 recorded no `ble.{name}`: {record}"))
    };
    assert_eq!(
        ble["connected"],
        serde_json::json!(true),
        "{id}: the body ran with no connection: {ble}"
    );
    assert!(count("adv_events") > 0, "{id}: nothing advertised: {ble}");
    assert!(
        count("connection_events") > 0,
        "{id}: the connection carried no event: {ble}"
    );
    let polls = count("polls_journaled");
    let notified = count("notified");
    assert!(
        polls >= F7_WINDOWS / 2,
        "{id}: the body journaled {polls} polls over its {F7_WINDOWS} windows, which is not a \
         stream: {ble}"
    );
    assert!(
        notified >= polls,
        "{id}: {polls} polls drew only {notified} notifications, so the stream stalled: {ble}"
    );
    assert_eq!(
        count("refused_steps"),
        0,
        "{id}: the queue overflowed: {ble}"
    );
    assert_eq!(
        ble["recent_failed_steps"],
        serde_json::json!(0),
        "{id}: a step of the stream did not end ok: {ble}"
    );
    assert!(
        count("tx_packets") > 0 && count("rx_packets") > 0,
        "{id}: no H4 traffic crossed the VHCI boundary: {ble}"
    );

    // 2. The demand ledger, per 100 ms virtual window.
    let metric = |name: &str| -> f64 {
        record["metrics"][name]
            .as_f64()
            .unwrap_or_else(|| panic!("{id}: F7 recorded no {name}: {stdout}"))
    };
    let windows = metric("windows");
    assert_eq!(
        windows, F7_WINDOWS as f64,
        "{id}: the 30 s body is {F7_WINDOWS} windows of 100 ms"
    );
    let virtual_s = metric("virtual_s");
    assert!(
        (virtual_s - 30.0).abs() <= 0.1,
        "{id}: the measured body is {virtual_s:.3} s virtual, not 30 s"
    );
    assert!(metric("busy_insns") > 0.0, "{id}: F7 executed nothing");
    let p95 = metric("demand_p95_mips");
    let max = metric("demand_max_mips");
    assert!(
        p95 > 0.0 && p95 <= max && max <= 160.0,
        "{id}: demand p95 {p95:.2} / max {max:.2} MIPS per 100 ms window is not a ledger of a \
         160 MHz core"
    );

    // 3. "Recorded" means in the history, not only on stdout.
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&history).expect("the history was written"))
            .expect("the history is JSON");
    let stored = doc["records"]
        .as_array()
        .expect("the history holds records")
        .iter()
        .find(|r| r["workload"] == "F7")
        .unwrap_or_else(|| panic!("{id}: no F7 record reached the history: {doc}"));
    assert_eq!(
        stored["metrics"]["demand_p95_mips"], record["metrics"]["demand_p95_mips"],
        "{id}: the stored record is not the one just measured"
    );
    assert_eq!(stored["ble"], record["ble"], "{id}: the stored BLE ledger");

    // 4. The host-time context, reported and not gated.
    let load = record["load_avg"].as_f64().unwrap_or(f64::NAN);
    let contended = record["contended"] == serde_json::json!(true);
    println!(
        "RAN {test}: F7 {windows} windows, {virtual_s:.2} s virtual, demand p95 {p95:.2} / max \
         {max:.2} MIPS per 100 ms window; BLE {} advertising events, {} connection events, \
         {notified} notifications from {polls} polls; wall {:.3} s at load average {load:.1} on \
         {} cores, contended={contended} (no native host-time budget is claimed)",
        count("adv_events"),
        count("connection_events"),
        metric("wall_s"),
        std::thread::available_parallelism().map_or(1, |n| n.get()),
    );
    std::fs::remove_dir_all(&dir).ok();
}

// ---------------------------------------------------------------------------------------------
// mbedTLS AES in DMA mode and the RSA block, against host-computed values
// ---------------------------------------------------------------------------------------------

/// The `probe_crypto` flash image, pinned by `tests/fw/manifest.toml`, or `None` after a skip.
fn probe_crypto_image(test: &str) -> Option<Vec<u8>> {
    pinned_probe_image(test, "probe_crypto")
}

/// The flash image of the probe `name`, pinned by `tests/fw/manifest.toml`, or `None` after a
/// skip.
fn pinned_probe_image(test: &str, name: &str) -> Option<Vec<u8>> {
    let pk = common::corpus_file_or_skip(test, common::PK, "FoloToy-AI-Passport-8MB.bin")?;
    let path = pk
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join(format!("probes/{name}-8MB.bin"));
    let Ok(flash) = std::fs::read(&path) else {
        common::skip(
            test,
            &format!("corpus/probes/{name}-8MB.bin is not built (xtask probes)"),
        );
        return None;
    };
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fw/manifest.toml"),
    )
    .expect("the probe manifest");
    let pinned = manifest
        .split("[[probe]]")
        .find(|block| block.contains(&format!("name = \"{name}\"")))
        .and_then(|block| {
            block.lines().find_map(|l| {
                l.strip_prefix("merged_sha256 = \"")
                    .map(|v| v.trim_end_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| panic!("tests/fw/manifest.toml pins {name}'s merged_sha256"));
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&flash),
        pinned,
        "{test}: the merged image is not the pinned build"
    );
    Some(flash)
}

/// An independent host reference for `probe_crypto`'s lines: the probe's xorshift32 input stream
/// and FNV-1a 64, FIPS-197 AES encryption (the S-box from the GF(2^8) inverse and the affine map,
/// not a table), SP 800-38A CBC and mbedTLS's CTR interface, SP 800-38D GCM, and schoolbook
/// big-integer multiplication and exponentiation. It shares no code with `periph/aes.rs` or
/// `periph/rsa.rs`; `tools/probe_crypto/host_expected.py` prints the same lines from Python's
/// `cryptography` package and integers, as a cross-check of this code.
mod host_crypto {
    /// `len` bytes of the probe's `fill`: xorshift32 from `seed`, each step's four bytes
    /// little-endian.
    pub fn fill(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        let mut out = Vec::with_capacity(len + 3);
        while out.len() < len {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            out.extend_from_slice(&x.to_le_bytes());
        }
        out.truncate(len);
        out
    }

    pub fn fnv(data: &[u8]) -> u64 {
        data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    }

    pub use pemu_loader::hex;

    /// The probe's `print_result` tail: `|fnv=..|head=..|tail=..`.
    pub fn result(data: &[u8]) -> String {
        format!(
            "|fnv={:016x}|head={}|tail={}",
            fnv(data),
            hex(&data[..16]),
            hex(&data[data.len() - 16..])
        )
    }

    fn xtime(b: u8) -> u8 {
        (b << 1) ^ if b & 0x80 != 0 { 0x1b } else { 0 }
    }

    fn gmul(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0;
        while b != 0 {
            if b & 1 != 0 {
                p ^= a;
            }
            a = xtime(a);
            b >>= 1;
        }
        p
    }

    /// The S-box entry of `b`: its inverse `b^254` in GF(2^8), then the affine map.
    fn sub(b: u8) -> u8 {
        let (mut inv, mut base, mut e) = (1u8, b, 254u32);
        while e != 0 {
            if e & 1 != 0 {
                inv = gmul(inv, base);
            }
            base = gmul(base, base);
            e >>= 1;
        }
        let mut s = inv;
        for k in 1..=4 {
            s ^= inv.rotate_left(k);
        }
        s ^ 0x63
    }

    pub struct Aes {
        round_keys: Vec<[u8; 16]>,
        sbox: [u8; 256],
    }

    impl Aes {
        pub fn new(key: &[u8]) -> Self {
            let sbox: [u8; 256] = std::array::from_fn(|i| sub(i as u8));
            let nk = key.len() / 4;
            let rounds = nk + 6;
            let mut w: Vec<[u8; 4]> = key
                .chunks(4)
                .map(|c| c.try_into().expect("whole words"))
                .collect();
            let mut rcon = 1u8;
            for i in nk..4 * (rounds + 1) {
                let mut t = w[i - 1];
                if i % nk == 0 {
                    t = [
                        sbox[t[1] as usize] ^ rcon,
                        sbox[t[2] as usize],
                        sbox[t[3] as usize],
                        sbox[t[0] as usize],
                    ];
                    rcon = xtime(rcon);
                } else if nk > 6 && i % nk == 4 {
                    t = t.map(|b| sbox[b as usize]);
                }
                let p = w[i - nk];
                w.push(std::array::from_fn(|j| p[j] ^ t[j]));
            }
            let round_keys = w
                .chunks(4)
                .map(|c| std::array::from_fn(|i| c[i / 4][i % 4]))
                .collect();
            Self { round_keys, sbox }
        }

        /// The forward cipher; the state is column-major, byte `row + 4 * column`.
        pub fn encrypt(&self, block: &[u8; 16]) -> [u8; 16] {
            let rounds = self.round_keys.len() - 1;
            let mut s: [u8; 16] = std::array::from_fn(|i| block[i] ^ self.round_keys[0][i]);
            for r in 1..=rounds {
                // SubBytes and ShiftRows: row `i % 4` of column `i / 4` comes from column
                // `(i / 4 + i % 4) % 4`.
                let mut t: [u8; 16] = std::array::from_fn(|i| {
                    self.sbox[s[i % 4 + 4 * ((i / 4 + i % 4) % 4)] as usize]
                });
                if r != rounds {
                    for c in 0..4 {
                        let a: [u8; 4] = t[4 * c..4 * c + 4].try_into().expect("a column");
                        for row in 0..4 {
                            t[4 * c + row] = gmul(a[row], 2)
                                ^ gmul(a[(row + 1) % 4], 3)
                                ^ a[(row + 2) % 4]
                                ^ a[(row + 3) % 4];
                        }
                    }
                }
                s = std::array::from_fn(|i| t[i] ^ self.round_keys[r][i]);
            }
            s
        }
    }

    /// SP 800-38A CBC encryption; the ciphertext and the IV mbedTLS hands back (the last block).
    pub fn cbc(aes: &Aes, iv: &[u8], data: &[u8]) -> (Vec<u8>, [u8; 16]) {
        let mut chain: [u8; 16] = iv.try_into().expect("16 bytes");
        let mut out = Vec::with_capacity(data.len());
        for block in data.chunks(16) {
            chain = aes.encrypt(&std::array::from_fn(|i| block[i] ^ chain[i]));
            out.extend_from_slice(&chain);
        }
        (out, chain)
    }

    /// mbedTLS `mbedtls_aes_crypt_ctr` from `nc_off` 0 and a zero stream block: the output, the
    /// final `nc_off`, nonce counter (128-bit big-endian increments) and stream block.
    pub fn ctr(aes: &Aes, nonce: &[u8], data: &[u8]) -> (Vec<u8>, usize, [u8; 16], [u8; 16]) {
        let mut counter: [u8; 16] = nonce.try_into().expect("16 bytes");
        let mut stream = [0u8; 16];
        let mut n = 0;
        let mut out = Vec::with_capacity(data.len());
        for &b in data {
            if n == 0 {
                stream = aes.encrypt(&counter);
                for byte in counter.iter_mut().rev() {
                    *byte = byte.wrapping_add(1);
                    if *byte != 0 {
                        break;
                    }
                }
            }
            out.push(b ^ stream[n]);
            n = (n + 1) % 16;
        }
        (out, n, counter, stream)
    }

    /// Multiplication in GF(2^128) with SP 800-38D's bit order (algorithm 1).
    fn gf128(x: u128, y: u128) -> u128 {
        let (mut z, mut v) = (0u128, y);
        for i in (0..128).rev() {
            if (x >> i) & 1 != 0 {
                z ^= v;
            }
            v = if v & 1 != 0 {
                (v >> 1) ^ (0xe1 << 120)
            } else {
                v >> 1
            };
        }
        z
    }

    /// SP 800-38D GCM encryption with a 96-bit IV: the ciphertext and the 128-bit tag.
    pub fn gcm(aes: &Aes, iv: &[u8], aad: &[u8], data: &[u8]) -> (Vec<u8>, [u8; 16]) {
        let h = u128::from_be_bytes(aes.encrypt(&[0; 16]));
        let mut j0 = [0u8; 16];
        j0[..12].copy_from_slice(iv);
        j0[15] = 1;
        let mut out = Vec::with_capacity(data.len());
        for (i, block) in data.chunks(16).enumerate() {
            let mut cb = j0;
            cb[12..].copy_from_slice(&(2 + i as u32).to_be_bytes());
            let ks = aes.encrypt(&cb);
            out.extend(block.iter().zip(ks).map(|(a, k)| a ^ k));
        }
        let mut y = 0u128;
        for part in [aad, &out[..]] {
            for block in part.chunks(16) {
                let mut padded = [0u8; 16];
                padded[..block.len()].copy_from_slice(block);
                y = gf128(y ^ u128::from_be_bytes(padded), h);
            }
        }
        let lengths = (((aad.len() as u128) * 8) << 64) | ((out.len() as u128) * 8);
        y = gf128(y ^ lengths, h);
        let tag = (y ^ u128::from_be_bytes(aes.encrypt(&j0))).to_be_bytes();
        (out, tag)
    }

    #[derive(Clone)]
    pub struct Big(pub Vec<u32>);

    impl Big {
        pub fn from_be(bytes: &[u8]) -> Self {
            Big(bytes
                .rchunks(4)
                .map(|c| c.iter().fold(0u32, |w, &b| (w << 8) | u32::from(b)))
                .collect())
        }

        /// `len` big-endian bytes, as `mbedtls_mpi_write_binary` writes them.
        pub fn to_be(&self, len: usize) -> Vec<u8> {
            let mut out = vec![0u8; len];
            for (i, limb) in self.0.iter().enumerate() {
                for (j, b) in limb.to_le_bytes().into_iter().enumerate() {
                    let k = 4 * i + j;
                    if k < len {
                        out[len - 1 - k] = b;
                    } else {
                        assert_eq!(b, 0, "the value fits {len} bytes");
                    }
                }
            }
            out
        }

        pub fn mul(&self, other: &Big) -> Big {
            let mut z = vec![0u32; self.0.len() + other.0.len()];
            for (i, &a) in self.0.iter().enumerate() {
                let mut carry = 0u64;
                for (j, &b) in other.0.iter().enumerate() {
                    let t = u64::from(a) * u64::from(b) + u64::from(z[i + j]) + carry;
                    z[i + j] = t as u32;
                    carry = t >> 32;
                }
                z[i + other.0.len()] = carry as u32;
            }
            Big(z)
        }

        fn bit(&self, i: usize) -> bool {
            self.0.get(i / 32).is_some_and(|w| (w >> (i % 32)) & 1 != 0)
        }

        /// `self mod n`, one bit at a time from the top: `r = 2r + bit`, less `n` when it
        /// reaches it.
        pub fn rem(&self, n: &Big) -> Big {
            let mut r = vec![0u32; n.0.len() + 1];
            let mut m = n.0.clone();
            m.push(0);
            for i in (0..32 * self.0.len()).rev() {
                let mut carry = u32::from(self.bit(i));
                for w in r.iter_mut() {
                    let next = *w >> 31;
                    *w = (*w << 1) | carry;
                    carry = next;
                }
                if r.iter().rev().cmp(m.iter().rev()) != std::cmp::Ordering::Less {
                    let mut borrow = 0i64;
                    for (w, &b) in r.iter_mut().zip(&m) {
                        let t = i64::from(*w) - i64::from(b) - borrow;
                        *w = t as u32;
                        borrow = i64::from(t < 0);
                    }
                }
            }
            r.truncate(n.0.len());
            Big(r)
        }

        /// `self ^ e mod n`, left to right.
        pub fn pow_mod(&self, e: &Big, n: &Big) -> Big {
            let mut acc = Big(vec![1]).rem(n);
            let base = self.rem(n);
            for i in (0..32 * e.0.len()).rev() {
                acc = acc.mul(&acc).rem(n);
                if e.bit(i) {
                    acc = acc.mul(&base).rem(n);
                }
            }
            acc
        }
    }

    /// The probe's 2048-bit modulus: seed 109 with the top and low bits set.
    fn modulus() -> Big {
        let mut n = fill(256, 109);
        n[0] |= 0x80;
        n[255] |= 0x01;
        Big::from_be(&n)
    }

    pub fn expected_lines() -> Vec<String> {
        let plain = fill(4101, 100);
        let mut lines = Vec::new();
        for (name, bits, key_seed) in [("aes_cbc128", 128, 101), ("aes_cbc256", 256, 103)] {
            let aes = Aes::new(&fill(bits / 8, key_seed));
            let (out, iv) = cbc(&aes, &fill(16, 102), &plain[..4096]);
            lines.push(format!(
                "AES|{name}|rc=0|len=4096{}|iv={}|dec_rc=0|dec_ok=1",
                result(&out),
                hex(&iv)
            ));
        }
        let aes = Aes::new(&fill(16, 104));
        let (out, off, nonce, mut stream) = ctr(&aes, &fill(16, 105), &plain);
        // IDF's DMA driver fills the stream block after a partial last block by running the zero-padded
        // tail as one more DMA block and copying all 16 bytes out (IDF v5.5.3
        // `mbedtls/port/aes/dma/esp_aes_dma_core.c`, `esp_aes_process_dma`, "Any leftover bytes"), so its
        // first `nc_off` bytes are ciphertext. Only bytes from `nc_off` on are read by the next call.
        stream[..off].copy_from_slice(&out[out.len() - off..]);
        lines.push(format!(
            "AES|aes_ctr128|rc=0|len=4101{}|nc_off={off}|nonce={}|stream={}",
            result(&out),
            hex(&nonce),
            hex(&stream)
        ));
        lines.push(format!(
            "AES|aes_ctr128_split|rc=0|len=4101|fnv={:016x}|same=1",
            fnv(&out)
        ));
        let aes = Aes::new(&fill(16, 106));
        let (out, tag) = gcm(&aes, &fill(12, 107), &fill(20, 108), &plain[..4096]);
        lines.push(format!(
            "AES|aes_gcm128|rc=0|len=4096{}|tag={}|dec_rc=0|dec_ok=1",
            result(&out),
            hex(&tag)
        ));
        let n = modulus();
        let mut x = fill(256, 110);
        x[0] &= 0x7f;
        let rsa = Big::from_be(&x).pow_mod(&Big(vec![65537]), &n).to_be(256);
        lines.push(format!("RSA|rsa_pub2048|rc=0{}", result(&rsa)));
        for (name, a_len, sa, b_len, sb) in [
            ("mpi_mul1024", 128, 113, 128, 114),
            ("mpi_mul1536", 192, 115, 192, 116),
            ("mpi_mul2048x1024", 256, 117, 128, 118),
        ] {
            let z = Big::from_be(&fill(a_len, sa))
                .mul(&Big::from_be(&fill(b_len, sb)))
                .to_be(a_len + b_len);
            lines.push(format!("MPI|{name}|rc=0{}", result(&z)));
        }
        let mut a = fill(256, 111);
        a[0] &= 0x7f;
        let z = Big::from_be(&a)
            .pow_mod(&Big::from_be(&fill(32, 112)), &n)
            .to_be(256);
        lines.push(format!("MPI|mpi_exp2048|rc=0{}", result(&z)));
        lines
    }

    /// The reference against published vectors, so a wrong reference cannot agree with a wrong
    /// model: SP 800-38A F.2.1 (CBC-AES128) and F.5.5 (CTR-AES256) first blocks, the GCM spec's
    /// test case 3 (AES-128, 96-bit IV, no AAD) and two small integer identities.
    pub fn self_check() {
        let unhex = |s: &str| -> Vec<u8> {
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
                .collect()
        };
        let aes = Aes::new(&unhex("2b7e151628aed2a6abf7158809cf4f3c"));
        let (out, _) = cbc(
            &aes,
            &unhex("000102030405060708090a0b0c0d0e0f"),
            &unhex("6bc1bee22e409f96e93d7e117393172a"),
        );
        assert_eq!(hex(&out), "7649abac8119b246cee98e9b12e9197d", "F.2.1");
        let aes = Aes::new(&unhex(
            "603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4",
        ));
        let (out, ..) = ctr(
            &aes,
            &unhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"),
            &unhex("6bc1bee22e409f96e93d7e117393172a"),
        );
        assert_eq!(hex(&out), "601ec313775789a5b7a7f504bbf3d228", "F.5.5");
        let aes = Aes::new(&unhex("feffe9928665731c6d6a8f9467308308"));
        let (out, tag) = gcm(
            &aes,
            &unhex("cafebabefacedbaddecaf888"),
            &[],
            &unhex(
                "d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a72\
                 1c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
            ),
        );
        assert_eq!(
            hex(&out[..16]),
            "42831ec2217774244b7221b784d0d49c",
            "GCM case 3"
        );
        assert_eq!(
            hex(&tag),
            "4d5c2af327cd64a62cf35abd2ba6fab4",
            "GCM case 3 tag"
        );
        let p = Big(vec![u32::MAX, u32::MAX]).mul(&Big(vec![u32::MAX, u32::MAX]));
        assert_eq!(hex(&p.to_be(16)), "fffffffffffffffe0000000000000001");
        let m = Big::from_be(&[0x01, 0x00, 0x01]).pow_mod(&Big(vec![3]), &Big(vec![1_000_003]));
        assert_eq!(m.0, vec![(65537u64.pow(3) % 1_000_003) as u32]);
    }
}

fn result_lines(console: &str) -> Vec<String> {
    console
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| ["AES|", "RSA|", "MPI|"].iter().any(|p| l.starts_with(p)))
        .map(str::to_string)
        .collect()
}

/// Every result line `probe_crypto` prints in the emulator (AES-CBC, CTR in one call and in two,
/// GCM, RSA-2048 public, MPI) is the value `host_crypto` computes. mbedTLS drives AES in DMA mode
/// with the completion interrupt (source 48) and RSA through `MODEXP_START`, `MULT_START` and
/// `MOD_MULT_START`, under the `device` profile. The reference is first checked against published
/// vectors and the committed QEMU oracle capture, which needs no data root.
#[test]
fn t1_m8_probe_crypto_matches_the_host_computed_values() {
    let test = "t1_m8_probe_crypto_matches_the_host_computed_values";
    host_crypto::self_check();
    let want = host_crypto::expected_lines();
    // The committed QEMU oracle capture (tests/fw/captures/README.md) is a third computation.
    let oracle = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fw/captures/probe_crypto.txt"),
    )
    .expect("the committed oracle capture");
    assert_eq!(
        result_lines(&oracle),
        want,
        "{test}: the host reference disagrees with the QEMU oracle capture"
    );
    let Some(image) = probe_crypto_image(test) else {
        return;
    };
    let flash = FlashImage::from_merged(&image).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(
        MachineConfig {
            profile: pemu_machine::config::TimingProfileId::Device,
            ..MachineConfig::default()
        },
        assets,
    )
    .expect("the image fits");
    let stop = pemu_machine::stops::MatcherId(0xC8);
    let out = m.run(RunLimits {
        until: Some(VTime::from_ms(20_000)),
        max_insns: None,
        stops: StopSet {
            matchers: vec![(
                stop,
                pemu_machine::stops::Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: pemu_machine::stops::LinePattern::Prefix("DONE|".into()),
                },
            )],
            ..StopSet::default()
        },
    });
    let text = console(&mut m);
    assert_eq!(out.reason, StopReason::Matcher(stop), "{test}:\n{text}");
    let got = result_lines(&text);
    let mut differ = Vec::new();
    for (i, w) in want.iter().enumerate() {
        let g = got.get(i).map_or("(none)", String::as_str);
        if g != w {
            differ.push(format!("  host:     {w}\n  emulator: {g}"));
        }
    }
    assert!(
        differ.is_empty() && got.len() == want.len(),
        "{test}: {} of {} lines differ ({} printed):\n{}\nconsole:\n{text}",
        differ.len(),
        want.len(),
        got.len(),
        differ.join("\n")
    );
    assert!(
        text.lines()
            .any(|l| l.trim_end() == "DONE|name=probe_crypto|status=ok"),
        "{test}: the probe did not finish ok:\n{text}"
    );
    assert!(
        !text.lines().any(|l| l.starts_with("FAIL|")),
        "{test}: the probe printed a FAIL line:\n{text}"
    );
    let applied = m.applied_wiring_by_kind();
    assert!(
        applied.aes_dma > 0,
        "{test}: no AES DMA run was applied, so mbedTLS did not use the DMA mode"
    );
    for line in &got {
        println!("  {line}");
    }
    println!(
        "RAN {test}: all {} result lines equal the host's and the oracle capture's; {} AES DMA \
         runs applied",
        want.len(),
        applied.aes_dma
    );
}

// ---------------------------------------------------------------------------------------------
// The probe_crypto result lines against the device capture, and the AES and
// RSA register traffic the class A rows of specs/blocks/{aes,rsa}.toml rest on
// ---------------------------------------------------------------------------------------------

/// The device capture of `probe_crypto`: `<id>-run1.log` and `<id>-run2.log` under the data
/// root's `captures/`. run1's first line is the tail of the run the reset cut.
const PROBE_CRYPTO_CAPTURE: &str = "device-probe_crypto-20260923T192126Z";

/// The probe's step functions in the pinned build, from its unstripped ELF (SHA-256 9b788903...,
/// the `ELF file SHA256` the capture prints), each with the calls `app_main` makes to it, so an
/// address that is not a step's entry fails.
const PROBE_CRYPTO_STEPS: [(u32, &str, usize); 6] = [
    (0x4200_722E, "aes_cbc", 2),
    (0x4200_7366, "aes_ctr", 1),
    (0x4200_7512, "aes_gcm", 1),
    (0x4200_7650, "rsa_public", 1),
    (0x4200_772A, "mpi_mul", 3),
    (0x4200_7806, "mpi_exp", 1),
];

/// The probe lines of one console (`PROBE|`, the `AES|`, `RSA|` and `MPI|` results and `DONE|`),
/// CR stripped.
fn probe_crypto_lines(console: &str) -> Vec<String> {
    console
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| {
            ["PROBE|", "AES|", "RSA|", "MPI|", "DONE|"]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(str::to_string)
        .collect()
}

/// The AES and RSA operations of one stretch of the run, from the MMIO trace: each trigger write
/// with the register values it ran with, and the result words read back, counted. With `sha`
/// set, the SHA block's mode writes, triggers and `SHA_H_MEM`/`SHA_M_MEM` words too.
#[derive(Default)]
struct CryptoTraffic {
    /// Whether SHA traffic is recorded (probe_crypto's only SHA traffic is the bootloader's verify).
    sha: bool,
    /// The last value written to each AES and RSA register; the blocks keep them across steps.
    written: std::collections::BTreeMap<u32, u32>,
    ops: std::collections::BTreeMap<String, usize>,
}

impl CryptoTraffic {
    const AES: u32 = 0x6003_A000;
    const SHA: u32 = 0x6003_B000;
    const RSA: u32 = 0x6003_C000;

    fn last(&self, addr: u32) -> u32 {
        self.written.get(&addr).copied().unwrap_or(0)
    }

    fn note(&mut self, what: String, n: u64) {
        *self.ops.entry(what).or_default() += usize::try_from(n).expect("a count");
    }

    fn record(&mut self, ev: pemu_core::trace::TraceEvent) {
        use pemu_core::trace::TraceEvent::{MmioRead, MmioWrite, PollRun};
        let (addr, val, write, n) = match ev {
            MmioWrite { addr, val, .. } => (addr, val, true, 1),
            MmioRead { addr, val, .. } => (addr, val, false, 1),
            PollRun {
                addr, val, count, ..
            } => (addr, val, false, count),
            _ => return,
        };
        let (aes, sha, rsa) = (Self::AES, Self::SHA, Self::RSA);
        if self.sha && (sha..sha + 0x1000).contains(&addr) {
            let off = addr - sha;
            let rw = if write { "written" } else { "read" };
            match off {
                0x40..0x60 => self.note(format!("SHA H_MEM word {rw}"), n),
                0x80..0xC0 => self.note(format!("SHA M_MEM word {rw}"), n),
                _ => {}
            }
            if !write {
                return;
            }
            self.written.insert(addr, val);
            let blocks = self.last(sha + 0x0C);
            match off {
                0x00 => self.note(format!("SHA MODE written {val}"), 1),
                0x10 => self.note("SHA START (block mode)".into(), 1),
                0x14 => self.note("SHA CONTINUE (block mode)".into(), 1),
                0x1C => self.note(format!("SHA DMA_START: DMA_BLOCK_NUM {blocks}"), 1),
                0x20 => self.note(format!("SHA DMA_CONTINUE: DMA_BLOCK_NUM {blocks}"), 1),
                _ => {}
            }
        } else if (aes..aes + 0x1000).contains(&addr) {
            let off = addr - aes;
            if (0x20..0x40).contains(&off) {
                self.note("AES TEXT_IN or TEXT_OUT access (typical mode)".into(), n);
            }
            if !write {
                if (0x50..0x60).contains(&off) {
                    self.note("AES IV_MEM word read back".into(), n);
                }
                return;
            }
            self.written.insert(addr, val);
            if off == 0x48 && val & 1 == 1 {
                let dma = self.last(aes + 0x90) & 1 == 1;
                let block_mode = self.last(aes + 0x94) & 7;
                let what = if !dma {
                    format!("AES typical block: MODE {}", self.last(aes + 0x40))
                } else if block_mode == 3 {
                    format!(
                        "AES DMA run: MODE {}, BLOCK_MODE 3 (CTR), BLOCK_NUM {}, INC_SEL {}",
                        self.last(aes + 0x40),
                        self.last(aes + 0x98),
                        self.last(aes + 0x9C)
                    )
                } else {
                    let name = ["ECB", "CBC", "OFB", "CTR", "CFB8", "CFB128", "6", "7"];
                    format!(
                        "AES DMA run: MODE {}, BLOCK_MODE {block_mode} ({}), BLOCK_NUM {}",
                        self.last(aes + 0x40),
                        name[block_mode as usize],
                        self.last(aes + 0x98)
                    )
                };
                self.note(what, 1);
            }
            if off == 0x9C {
                self.note(format!("AES INC_SEL written {val}"), 1);
            }
        } else if (rsa..rsa + 0x1000).contains(&addr) {
            let off = addr - rsa;
            if !write {
                if (0x200..0x380).contains(&off) {
                    self.note("RSA Z_MEM word read back".into(), n);
                }
                return;
            }
            self.written.insert(addr, val);
            let length = self.last(rsa + 0x804);
            match off {
                0x80C if val & 1 == 1 => self.note(
                    format!(
                        "RSA MODEXP_START: LENGTH {length}, SEARCH_ENABLE {}, SEARCH_POS {}",
                        self.last(rsa + 0x824),
                        self.last(rsa + 0x828)
                    ),
                    1,
                ),
                0x810 if val & 1 == 1 => {
                    self.note(format!("RSA MOD_MULT_START: LENGTH {length}"), 1);
                }
                0x814 if val & 1 == 1 => self.note(format!("RSA MULT_START: LENGTH {length}"), 1),
                _ => {}
            }
        }
    }

    fn take(&mut self) -> Vec<String> {
        std::mem::take(&mut self.ops)
            .into_iter()
            .map(|(what, n)| format!("{n} x {what}"))
            .collect()
    }
}

/// The AES and RSA traffic of each probe step (sorted `CryptoTraffic` lines) from this test's own
/// run: the evidence the class A rows of `specs/blocks/aes.toml` and `rsa.toml` cite.
///
/// `rsa_pub2048` never starts `MODEXP_START`: mbedTLS 3.6's `mbedtls_rsa_public` calls
/// `mbedtls_mpi_exp_mod_unsafe`, which IDF does not accelerate, so the RSA block serves only its
/// reductions (`MULT_START` at 2 words and the `MOD_MULT_START` failover at 66); the one
/// `MODEXP_START` is `mpi_exp2048`'s, as are the search registers.
const PROBE_CRYPTO_TRAFFIC: [(&str, &[&str]); 9] = [
    (
        "aes_cbc#1 (aes_cbc128)",
        &[
            "1 x AES DMA run: MODE 0, BLOCK_MODE 1 (CBC), BLOCK_NUM 256",
            "1 x AES DMA run: MODE 4, BLOCK_MODE 1 (CBC), BLOCK_NUM 256",
            "8 x AES IV_MEM word read back",
        ],
    ),
    (
        "aes_cbc#2 (aes_cbc256)",
        &[
            "1 x AES DMA run: MODE 2, BLOCK_MODE 1 (CBC), BLOCK_NUM 256",
            "1 x AES DMA run: MODE 6, BLOCK_MODE 1 (CBC), BLOCK_NUM 256",
            "8 x AES IV_MEM word read back",
        ],
    ),
    (
        "aes_ctr#1 (aes_ctr128 from the 257-block run, aes_ctr128_split from the 63 and 194)",
        &[
            "1 x AES DMA run: MODE 4, BLOCK_MODE 3 (CTR), BLOCK_NUM 194, INC_SEL 0",
            "1 x AES DMA run: MODE 4, BLOCK_MODE 3 (CTR), BLOCK_NUM 257, INC_SEL 0",
            "1 x AES DMA run: MODE 4, BLOCK_MODE 3 (CTR), BLOCK_NUM 63, INC_SEL 0",
            "3 x AES INC_SEL written 0",
            "12 x AES IV_MEM word read back",
        ],
    ),
    (
        "aes_gcm#1 (aes_gcm128)",
        &[
            "1 x AES DMA run: MODE 0, BLOCK_MODE 0 (ECB), BLOCK_NUM 1",
            "2 x AES DMA run: MODE 4, BLOCK_MODE 3 (CTR), BLOCK_NUM 1, INC_SEL 0",
            "2 x AES DMA run: MODE 4, BLOCK_MODE 3 (CTR), BLOCK_NUM 256, INC_SEL 0",
            "4 x AES INC_SEL written 0",
            "16 x AES IV_MEM word read back",
        ],
    ),
    (
        "rsa_public#1 (rsa_pub2048)",
        &[
            "64 x RSA MOD_MULT_START: LENGTH 65",
            "167 x RSA MULT_START: LENGTH 3",
            "4725 x RSA Z_MEM word read back",
        ],
    ),
    (
        "mpi_mul#1 (mpi_mul1024)",
        &[
            "1 x RSA MULT_START: LENGTH 63",
            "64 x RSA Z_MEM word read back",
        ],
    ),
    (
        "mpi_mul#2 (mpi_mul1536)",
        &[
            "1 x RSA MULT_START: LENGTH 95",
            "96 x RSA Z_MEM word read back",
        ],
    ),
    (
        "mpi_mul#3 (mpi_mul2048x1024)",
        &[
            "1 x RSA MOD_MULT_START: LENGTH 95",
            "96 x RSA Z_MEM word read back",
        ],
    ),
    (
        "mpi_exp#1 (mpi_exp2048)",
        &[
            "1 x RSA MODEXP_START: LENGTH 63, SEARCH_ENABLE 1, SEARCH_POS 255",
            "64 x RSA MOD_MULT_START: LENGTH 65",
            "167 x RSA MULT_START: LENGTH 3",
            "4789 x RSA Z_MEM word read back",
        ],
    ),
];

/// Runs a probe image under the `device` profile until a `DONE|` line, with the MMIO trace on and
/// a breakpoint at each of `entries`; returns the crypto traffic before the first step (`boot`)
/// and of each step (`<step>#<call>`), and the console. Panics unless each entry is reached
/// exactly the number of times its row says.
fn traced_probe_steps(
    test: &str,
    image: &[u8],
    entries: &[(u32, &str, usize)],
    sha: bool,
) -> (Vec<(String, Vec<String>)>, String) {
    use pemu_core::trace::TraceKinds;
    use pemu_machine::config::TraceCfg;
    use pemu_machine::stops::{LinePattern, Matcher, MatcherId};

    let flash = FlashImage::from_merged(image).expect("a probe image parses");
    let assets = Assets::with_bundled_rom(flash, None, None, EfuseImage::synth(0))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(
        MachineConfig {
            profile: pemu_machine::config::TimingProfileId::Device,
            trace: TraceCfg {
                kinds: Some(TraceKinds::MMIO_READ.union(TraceKinds::MMIO_WRITE)),
                recent: 1 << 18,
            },
            ..MachineConfig::default()
        },
        assets,
    )
    .expect("the image fits");
    let done = MatcherId(0xC9);
    let stops = StopSet {
        breakpoints: entries.iter().map(|s| s.0).collect(),
        matchers: vec![(
            done,
            Matcher::Serial {
                stream: SerialStream::UsjTx,
                pattern: LinePattern::Prefix("DONE|".into()),
            },
        )],
        ..StopSet::default()
    };
    // Short enough that the replay window never drops a record between two drains.
    let slice = VTime::from_ms(20);
    let limit = VTime::from_ms(20_000);
    let mut traffic = CryptoTraffic {
        sha,
        ..CryptoTraffic::default()
    };
    let mut calls = vec![0usize; entries.len()];
    let mut steps: Vec<(String, Vec<String>)> = Vec::new();
    let mut step = "boot".to_string();
    let mut cursor = 0u64;
    loop {
        let out = m.run(RunLimits {
            until: Some(VTime((m.now().0 + slice.0).min(limit.0))),
            max_insns: None,
            stops: stops.clone(),
        });
        let trace = m.trace();
        assert!(
            trace.tail() <= cursor,
            "{test}: the trace window lost records {cursor}..{}",
            trace.tail()
        );
        let skip = usize::try_from(cursor - trace.tail()).expect("a window index");
        for r in trace.records().skip(skip) {
            traffic.record(r.ev);
        }
        cursor = trace.head();
        match out.reason {
            StopReason::Breakpoint(pc) => {
                let i = entries
                    .iter()
                    .position(|s| s.0 == pc)
                    .expect("a step breakpoint");
                calls[i] += 1;
                let next = format!("{}#{}", entries[i].1, calls[i]);
                steps.push((std::mem::replace(&mut step, next), traffic.take()));
            }
            StopReason::Until if m.now() < limit => {}
            StopReason::Matcher(id) if id == done => break,
            other => panic!("{test}: the run stopped {other:?}:\n{}", console(&mut m)),
        }
    }
    steps.push((step, traffic.take()));
    let want_calls: Vec<usize> = entries.iter().map(|s| s.2).collect();
    assert_eq!(
        calls, want_calls,
        "{test}: the step breakpoints were not reached as app_main calls them"
    );
    for (name, ops) in &steps {
        println!("  {name}:");
        for op in ops {
            println!("    {op}");
        }
    }
    (steps, console(&mut m))
}

/// `probe_crypto` in the emulator prints what the device printed in [`PROBE_CRYPTO_CAPTURE`] line
/// for line, and its register traffic step by step is [`PROBE_CRYPTO_TRAFFIC`], so a class A row
/// is cited only for the lines whose operations use it.
#[test]
fn t1_m8_probe_crypto_matches_the_device_capture() {
    let test = "t1_m8_probe_crypto_matches_the_device_capture";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(
            test,
            &format!("no data root, so no capture {PROBE_CRYPTO_CAPTURE}"),
        );
        return;
    };
    let mut runs = Vec::new();
    for n in 1..=2 {
        let name = format!("{PROBE_CRYPTO_CAPTURE}-run{n}.log");
        let Ok(bytes) = std::fs::read(root.join("captures").join(&name)) else {
            common::skip(test, &format!("no capture {name}"));
            return;
        };
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let text = if n == 1 {
            text.split_once('\n')
                .map_or("", |(_, rest)| rest)
                .to_string()
        } else {
            text
        };
        runs.push(probe_crypto_lines(&text));
    }
    let device = runs[1].clone();
    assert_eq!(
        runs[0], device,
        "{test}: the two device runs of {PROBE_CRYPTO_CAPTURE} differ"
    );
    assert_eq!(
        device
            .iter()
            .filter(|l| !l.starts_with("PROBE|") && !l.starts_with("DONE|"))
            .count(),
        10,
        "{test}: {PROBE_CRYPTO_CAPTURE} does not hold the ten result lines:\n{device:#?}"
    );
    assert_eq!(
        device.last().map(String::as_str),
        Some("DONE|name=probe_crypto|status=ok"),
        "{test}: {PROBE_CRYPTO_CAPTURE} did not finish ok"
    );

    let Some(image) = probe_crypto_image(test) else {
        return;
    };
    let (steps, text) = traced_probe_steps(test, &image, &PROBE_CRYPTO_STEPS, false);

    let got = probe_crypto_lines(&text);
    let mut differ = Vec::new();
    for i in 0..device.len().max(got.len()) {
        let d = device.get(i).map_or("(none)", String::as_str);
        let g = got.get(i).map_or("(none)", String::as_str);
        if d != g {
            differ.push(format!("  device:   {d}\n  emulator: {g}"));
        }
    }
    assert!(
        differ.is_empty(),
        "{test}: {} of {} lines differ from {PROBE_CRYPTO_CAPTURE}:\n{}",
        differ.len(),
        device.len(),
        differ.join("\n")
    );

    assert_eq!(
        steps[0].1,
        Vec::<String>::new(),
        "{test}: AES or RSA traffic before the first step"
    );
    let got_traffic: Vec<(&str, Vec<String>)> = steps[1..]
        .iter()
        .map(|(n, ops)| (n.as_str(), ops.clone()))
        .collect();
    let want_traffic: Vec<(&str, Vec<String>)> = PROBE_CRYPTO_TRAFFIC
        .iter()
        .map(|(n, ops)| {
            (
                n.split(' ').next().expect("a step name"),
                ops.iter().map(|s| (*s).to_string()).collect(),
            )
        })
        .collect();
    assert_eq!(
        got_traffic, want_traffic,
        "{test}: the AES and RSA traffic of the steps is not the table the class A rows cite"
    );
    let aes_runs: usize = steps
        .iter()
        .flat_map(|(_, ops)| ops)
        .filter(|op| op.contains(" x AES DMA run"))
        .map(|op| {
            op.split(' ')
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .unwrap_or(0)
        })
        .sum();
    println!(
        "RAN {test}: all {} probe lines equal both device runs of {PROBE_CRYPTO_CAPTURE}; the \
         AES and RSA traffic of the {} steps ({aes_runs} AES DMA runs) is the table the class A \
         rows cite",
        device.len(),
        steps.len() - 1
    );
}

/// The three-run device capture of `probe_timing` (a rebuild on the device's partition table,
/// banner `ELF file SHA256: 82c0e91c1...`) under the data root's `captures/`. run1 opens with a
/// cut line.
const PROBE_TIMING_CAPTURE: &str = "device-probe_timing-20260923T101040Z";

/// `time_sha256` and the step after it in the pinned `probe_timing` build (`elf_sha256`
/// e32f1f00...), with the one call `app_main` makes to each.
const PROBE_TIMING_SHA_STEPS: [(u32, &str, usize); 2] = [
    (0x4200_73C4, "time_sha256", 1),
    (0x4200_750E, "time_erase", 1),
];

/// The SHA traffic of `time_sha256` (1 MB in 256 updates of 4 KB, then the finish): the evidence
/// the class A rows of `specs/blocks/sha.toml` cite. Each update is DMA runs of 62 and 2 blocks
/// behind a `SHA_H_MEM` state write (none before the first); the finish hashes the padding block
/// in block mode through `SHA_M_MEM`.
const PROBE_TIMING_SHA_TRAFFIC: [&str; 8] = [
    "1 x SHA CONTINUE (block mode)",
    "256 x SHA DMA_CONTINUE: DMA_BLOCK_NUM 2",
    "255 x SHA DMA_CONTINUE: DMA_BLOCK_NUM 62",
    "1 x SHA DMA_START: DMA_BLOCK_NUM 62",
    "2056 x SHA H_MEM word read",
    "2048 x SHA H_MEM word written",
    "257 x SHA MODE written 2",
    "16 x SHA M_MEM word written",
];

/// Every whole `sha256_1m` digest of a console; run1's cut first line is not taken.
fn sha256_1m_digests(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| l.starts_with("TIMING|sha256_1m|"))
        .filter_map(|l| {
            l.trim_end_matches('\r')
                .split('|')
                .find_map(|f| f.strip_prefix("digest="))
        })
        .filter(|d| d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .map(str::to_string)
        .collect()
}

/// `probe_timing`'s `sha256_1m` digest in the emulator is the one all three device runs print,
/// and its `time_sha256` traffic is [`PROBE_TIMING_SHA_TRAFFIC`]. The device ran a rebuild whose
/// SHA and mbedTLS sources are the pinned build's, so the traffic is read from the pinned build.
#[test]
fn t1_m8_the_probe_timing_digest_rests_on_this_sha_traffic() {
    let test = "t1_m8_the_probe_timing_digest_rests_on_this_sha_traffic";
    let Ok(root) = pemu_testkit::corpus::data_root_from_env() else {
        common::skip(
            test,
            &format!("no data root, so no capture {PROBE_TIMING_CAPTURE}"),
        );
        return;
    };
    let mut device = Vec::new();
    for n in 1..=3 {
        let name = format!("{PROBE_TIMING_CAPTURE}-run{n}.log");
        let Ok(bytes) = std::fs::read(root.join("captures").join(&name)) else {
            common::skip(test, &format!("no capture {name}"));
            return;
        };
        let found = sha256_1m_digests(&String::from_utf8_lossy(&bytes));
        assert_eq!(found.len(), 1, "{test}: {name} holds one whole digest");
        device.extend(found);
    }
    assert!(
        device.windows(2).all(|w| w[0] == w[1]),
        "{test}: the device runs disagree: {device:?}"
    );
    let Some(image) = pinned_probe_image(test, "probe_timing") else {
        return;
    };
    let (steps, text) = traced_probe_steps(test, &image, &PROBE_TIMING_SHA_STEPS, true);
    assert_eq!(
        sha256_1m_digests(&text),
        vec![device[0].clone()],
        "{test}: the emulator's sha256_1m digest is not the device's"
    );
    let sha_step = steps
        .iter()
        .find(|(name, _)| name == "time_sha256#1")
        .map(|(_, ops)| ops.clone())
        .expect("the time_sha256 step");
    assert_eq!(
        sha_step,
        PROBE_TIMING_SHA_TRAFFIC.map(str::to_string).to_vec(),
        "{test}: the SHA traffic of time_sha256 is not the table the class A rows of sha.toml cite"
    );
    println!(
        "RAN {test}: sha256_1m prints the digest of all three runs of {PROBE_TIMING_CAPTURE} \
         ({}...), and the SHA traffic of time_sha256 is the table the class A rows cite",
        &device[0][..8]
    );
}

// ---------------------------------------------------------------------------------------------
// Binding without an ELF
// ---------------------------------------------------------------------------------------------

/// Corpus builds with an app ELF that link a radio: id, image file, ELF file.
const RADIO_CORPUS: [(&str, &str, &str); 6] = [
    (
        "pk",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
    ("pkgatt", "merged-binary.bin", "radio_pkgatt.elf"),
    ("scan3", "merged-binary.bin", "radio_scan3probe.elf"),
    ("probe2", "probe2-merged.bin", "radio_heapprobe.elf"),
    (
        "official",
        "FoloToy-AI-Passport-8MB.bin",
        "FoloToy-AI-Passport.elf",
    ),
    ("demo", "demo-merged.bin", "FoloToy-AI-Passport.elf"),
];

/// Device probes that link a radio, pinned by `tests/fw/manifest.toml`.
const RADIO_PROBES: [&str; 5] = [
    "probe_wifi_http",
    "probe_wifi_conn",
    "probe_wifi_assoc",
    "probe_campaign_radio",
    "hle_probe",
];

fn radio_probe_files(test: &str, name: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    let official =
        common::corpus_file_or_skip(test, common::OFFICIAL, "FoloToy-AI-Passport-8MB.bin")?;
    let probes = official
        .ancestors()
        .nth(2)
        .expect("a corpus file sits under corpus/<id>/")
        .join("probes");
    let read = |path: std::path::PathBuf| match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(_) => {
            common::skip(test, &format!("probe {name} is not built (xtask probes)"));
            None
        }
    };
    let flash = read(probes.join(format!("{name}-8MB.bin")))?;
    let elf = read(probes.join(format!("build/{name}/{name}.elf")))?;
    let manifest =
        std::fs::read_to_string(workspace().join("tests/fw/manifest.toml")).expect("the manifest");
    let block = manifest
        .split("[[probe]]")
        .find(|b| b.contains(&format!("name = \"{name}\"")))
        .unwrap_or_else(|| panic!("tests/fw/manifest.toml has probe {name}"));
    let pinned = |key: &str| {
        block
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix(&format!("{key} = \""))
                    .map(|v| v.trim_end_matches('"').to_string())
            })
            .unwrap_or_else(|| panic!("the manifest pins {name}'s {key}"))
    };
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&flash),
        pinned("merged_sha256"),
        "{test}: probe {name}'s image is not the pinned one"
    );
    assert_eq!(
        pemu_testkit::corpus::sha256_hex(&elf),
        pinned("elf_sha256"),
        "{test}: probe {name}'s ELF is not the pinned one"
    );
    Some((flash, elf))
}

fn radio_builds(test: &str) -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    for (id, bin, elf) in RADIO_CORPUS {
        let (Some(bin), Some(elf)) = (
            common::corpus_file_or_skip(test, id, bin),
            common::corpus_file_or_skip(test, id, elf),
        ) else {
            continue;
        };
        out.push((
            id,
            std::fs::read(bin).expect("readable"),
            std::fs::read(elf).expect("readable"),
        ));
    }
    for name in RADIO_PROBES {
        if let Some((flash, elf)) = radio_probe_files(test, name) {
            out.push((name, flash, elf));
        }
    }
    out
}

fn module_hooks(
    m: &Machine,
    module: pemu_hle::hooks::ModuleIndex,
) -> std::collections::BTreeMap<u32, pemu_hle::hooks::HookId> {
    m.hle_binding()
        .set
        .iter()
        .filter(|(_, id)| {
            pemu_hle::hooks::HookRef::from_id(*id).is_some_and(|r| r.module == module)
        })
        .collect()
}

/// For every corpus build with an ELF that links a radio, the bare flash image binds what the ELF
/// binds: the same hooks at the same pcs, core observe hooks, and call and data addresses. A
/// module the ELF binds while some names it reads are not linked is refused from the image,
/// naming them, because an image cannot show a function is absent.
#[test]
fn t1_m8_elfless_every_radio_build_binds_from_its_image_as_its_elf_does() {
    let test = "t1_m8_elfless_every_radio_build_binds_from_its_image_as_its_elf_does";
    let builds = radio_builds(test);
    if builds.is_empty() {
        return;
    }
    for (id, flash, elf) in &builds {
        let with = machine(flash, elf, MachineConfig::default());
        let without = common::image_machine(flash);
        let elf_info = with.assets().app_elf.clone().expect("an ELF");
        let recovered = without
            .assets()
            .recovered_symbols()
            .expect("a boot app to recover from");
        let (a, b) = (with.hle_binding(), without.hle_binding());
        for module in pemu_radio::modules() {
            let name = module.name();
            let wants = module.image_symbols();
            let unlinked: Vec<&str> = wants
                .hooks
                .iter()
                .chain(&wants.required)
                .copied()
                .filter(|h| elf_info.symbols.lookup(h).is_none())
                .collect();
            let index = if name == "ble" {
                pemu_radio::ble::hle::BLE_MODULE
            } else {
                pemu_radio::wifi::hle::WIFI_MODULE
            };
            match a.record.features.get(name) {
                Some(FeatureStatus::Bound) if unlinked.is_empty() => {
                    assert_eq!(
                        b.record.features.get(name),
                        Some(&FeatureStatus::Bound),
                        "{id} {name}: {:?}",
                        b.mismatches
                    );
                    assert_eq!(
                        module_hooks(&without, index),
                        module_hooks(&with, index),
                        "{id} {name}: the hooks"
                    );
                    for sym in wants.required.iter().chain(&wants.hooks) {
                        assert_eq!(
                            recovered.elf.symbols.addr_of(sym),
                            elf_info.symbols.addr_of(sym),
                            "{id}: {sym}"
                        );
                    }
                }
                Some(FeatureStatus::Bound) => {
                    assert_eq!(
                        b.record.features.get(name),
                        Some(&FeatureStatus::UnsupportedImage),
                        "{id} {name}"
                    );
                    let named: Vec<&str> = b
                        .mismatches
                        .iter()
                        .find(|(n, _)| *n == name)
                        .map(|(_, why)| why.iter().map(|m| m.symbol.as_str()).collect())
                        .unwrap_or_default();
                    let mut want = unlinked.clone();
                    want.sort_unstable();
                    assert_eq!(
                        named, want,
                        "{id} {name}: the refusal names the unlinked hooks"
                    );
                    assert!(module_hooks(&without, index).is_empty(), "{id} {name}");
                    let init = wants.hooks[0];
                    let pc = elf_info.symbols.addr_of(init).expect("the init is linked");
                    assert_eq!(
                        without.tripwires().at(pc).map(|t| t.0),
                        Some(TripKind::DisabledFeature),
                        "{id} {name}: {init} is the tripwire"
                    );
                }
                status => assert_eq!(b.record.features.get(name), status, "{id} {name}"),
            }
        }
        let observe = |m: &Machine| -> Vec<(u32, pemu_hle::hooks::HookId)> {
            module_hooks(m, pemu_hle::hooks::ModuleIndex::CORE)
                .into_iter()
                .filter(|(_, id)| {
                    matches!(
                        pemu_hle::hooks::HookRef::from_id(*id).map(|r| r.kind),
                        Some(pemu_hle::hooks::HookKind::Observe(_))
                    )
                })
                .collect()
        };
        assert_eq!(observe(&without), observe(&with), "{id}: the observe hooks");
        println!(
            "RAN {test}: {id}: ble {:?} wifi {:?}; tripwires {} with the ELF, {} from the image",
            b.record.features.get("ble").map(|s| s.receipt_word()),
            b.record.features.get("wifi").map(|s| s.receipt_word()),
            with.tripwires().len(),
            without.tripwires().len()
        );
    }
}

/// Fail closed without an ELF: on the bare `pk` image a changed BLE function, inside or past the
/// first 32 bytes, and an app descriptor of another IDF version each refuse BLE with a reason
/// naming the cause, and the image stops at the `esp_bt_controller_init` tripwire.
#[test]
fn t1_m8_elfless_a_changed_function_or_another_idf_refuses_ble_from_the_image() {
    let test = "t1_m8_elfless_a_changed_function_or_another_idf_refuses_ble_from_the_image";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let control = common::image_machine(&flash);
    assert_eq!(
        control.hle_binding().record.features.get("ble"),
        Some(&FeatureStatus::Bound),
        "{test}: {:?}",
        control.hle_binding().mismatches
    );
    let refused = |patch: &dyn Fn(&mut [u8], &mut [u8])| {
        let (mut f, mut e) = (flash.clone(), elf.clone());
        patch(&mut f, &mut e);
        let m = common::image_machine(&f);
        assert_eq!(
            m.hle_binding().record.features.get("ble"),
            Some(&FeatureStatus::UnsupportedImage),
            "{test}"
        );
        let why = format!("{:?}", m.hle_binding().mismatches);
        (m, why)
    };
    // `ori a5, a5, 4` at +0x36 of pk's enable becomes `ori a5, a5, 5`: only the image path's body
    // check reads it.
    let (_, why) = refused(&|f, e| {
        patch_symbol(f, e, "esp_bt_controller_enable", 0x36, 4, |b| {
            assert_eq!(b, [0x93, 0xe7, 0x47, 0x00], "pk's ori a5, a5, 4");
            b[2] ^= 0x10;
        })
    });
    assert!(
        why.contains("esp_bt_controller_enable") && why.contains("Missing"),
        "{test}: {why}"
    );
    let (_, why) = refused(&|f, e| {
        patch_symbol(f, e, "esp_app_desc", 112, 32, |b| {
            b.fill(0);
            b[..6].copy_from_slice(b"v5.5.2");
        })
    });
    assert!(
        why.contains("IdfVersion") && why.contains("v5.5.2"),
        "{test}: {why}"
    );
    let (mut m, why) = refused(&|f, e| alter_first_32_bytes(f, e, "esp_vhci_host_send_packet"));
    assert!(why.contains("esp_vhci_host_send_packet"), "{test}: {why}");
    let init = ElfInfo::parse(&elf)
        .expect("parses")
        .symbols
        .addr_of("esp_bt_controller_init")
        .expect("linked");
    let out = m.run(one_second());
    let StopReason::Tripwire(report) = &out.reason else {
        panic!("{test}: the refused image ended {:?}", out.reason);
    };
    assert_eq!(
        (report.kind, report.pc, report.feature),
        (TripKind::DisabledFeature, init, Some("ble")),
        "{test}"
    );
    println!("RAN {test}: a body change, a head change and v5.5.2 each refuse BLE by name");
}

/// The bare `pk` image runs its first second exactly as `pk` with its ELF does, on both executors.
#[test]
fn t1_m8_elfless_pk_without_its_elf_runs_its_first_second_as_with_it() {
    let test = "t1_m8_elfless_pk_without_its_elf_runs_its_first_second_as_with_it";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    for executor in [Executor::Engine, Executor::Reference] {
        let mut with = machine(&flash, &elf, MachineConfig::default());
        let mut without = common::image_machine(&flash);
        with.set_executor(executor);
        without.set_executor(executor);
        let (a, b) = (with.run(one_second()), without.run(one_second()));
        assert_eq!(
            format!("{:?}", b.reason),
            format!("{:?}", a.reason),
            "{test} {executor:?}"
        );
        let (ca, cb) = (console(&mut with), console(&mut without));
        assert!(cb.contains("BLE_INIT: Bluetooth MAC"), "{test}: {cb}");
        assert_eq!(cb, ca, "{test} {executor:?}: the consoles differ");
    }
    println!("RAN {test}: 1 s of pk without its ELF equals 1 s with it on both executors");
}

/// The build a machine reports (`status` `build`, the page header) is the firmware's own
/// `esp_app_desc_t`, with or without the ELF, and its hash is the `ELF file SHA256:` the firmware
/// prints at start.
#[test]
fn t1_m8_elfless_the_reported_build_is_the_one_the_firmware_prints() {
    let test = "t1_m8_elfless_the_reported_build_is_the_one_the_firmware_prints";
    let Some((flash, elf)) = pk_files(test) else {
        return;
    };
    let with = machine(&flash, &elf, MachineConfig::default());
    let mut without = common::image_machine(&flash);
    let (a, b) = (
        pemu_machine::MachineApi::app_desc(&with).expect("pk carries an app descriptor"),
        pemu_machine::MachineApi::app_desc(&without).expect("the image alone carries it too"),
    );
    assert_eq!(a, b, "{test}");
    let hex: String = a
        .app_elf_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    without.run(one_second());
    let printed = console(&mut without);
    let line = printed
        .lines()
        .find(|line| line.contains("ELF file SHA256:"))
        .unwrap_or_else(|| panic!("{test}: no ELF SHA line in {printed}"));
    let shown = line
        .split("ELF file SHA256:")
        .nth(1)
        .map(|rest| rest.trim().trim_end_matches("..."))
        .unwrap_or_default();
    assert!(
        shown.len() >= 8 && hex.starts_with(shown),
        "{test}: {shown} vs {hex}"
    );
    println!("RAN {test}: pk reports build {shown}, as its console prints");
}
