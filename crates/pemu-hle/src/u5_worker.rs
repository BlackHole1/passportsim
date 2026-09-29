//! The U5 (polled) worker on the synthetic firmware of `crate::rtos_model`: creation, park,
//! timeout and wake reasons are production `HleCore` code; only the IDF FreeRTOS callee bodies
//! and the task switch are modelled.

use pemu_core::snap::SnapSection;

use crate::continuation::HleSection;
use crate::core::script::ScriptHost;
use crate::core::{HleCore, Step};
use crate::guest_call::{GuestView, HleAction, HleErrorKind, SP};
use crate::hooks::{HandlerKind, HookKind, HookRef, ModuleIndex};
use crate::magic::MagicKind;
use crate::rtos_model::{CALLS, Firmware, INIT_RA, SEMAPHORE, TCB_WORKER, Task};
use crate::test_guest::TEXT;
use crate::worker::{
    PORT_MAX_DELAY, WakeMode, WakeReason, WorkerConfig, WorkerProfile, bt_controller_profile,
};

#[test]
fn a_synthetic_firmware_creates_the_worker_parks_it_and_wakes_it_from_a_polled_semaphore() {
    let mut fw = Firmware::new(WakeMode::U5Polling);
    let init = fw.boot_radio();

    assert_eq!(
        init,
        Step::Returned {
            pc: INIT_RA,
            a0: 0,
            a1: 0
        }
    );
    assert_eq!(fw.host.handle, Some(TCB_WORKER), "&handle came back");
    let (entry, param) = fw.m.created.expect("created");
    assert_eq!(
        entry, fw.core.engine.pcs.bt_worker,
        "the task enters at the magic PC"
    );
    assert_eq!(param, SEMAPHORE, "pvParameters is the semaphore");

    let worker = fw.core.worker(MagicKind::BtWorker).expect("recorded");
    assert_eq!((worker.task, worker.semaphore), (TCB_WORKER, SEMAPHORE));
    assert_eq!(fw.m.wait_ticks, [20], "U5 polls every 20 ticks");
    assert!(fw.m.worker_blocked);
    assert_eq!(
        fw.core.outstanding(),
        1,
        "the wait is the one outstanding call"
    );
    assert_eq!(fw.m.running, Task::Main);

    fw.resume_worker(false);
    assert_eq!(fw.host.wakes, [WakeReason::Timeout]);
    assert_eq!(fw.m.wait_ticks, [20, 20], "and the worker parks again");

    // U5 raises nothing: the next poll finds the event.
    fw.post(7);
    assert!(fw.g.raised.is_empty(), "U5 raises no interrupt line");
    fw.resume_worker(false);
    assert_eq!(fw.host.wakes, [WakeReason::Timeout, WakeReason::Event]);
    assert_eq!(fw.delivered, [7]);
    assert_eq!(
        fw.m.wait_ticks,
        [20, 20, 20],
        "back to polling once it was seen"
    );

    // An event is reported once.
    fw.resume_worker(false);
    assert_eq!(fw.host.wakes.last(), Some(&WakeReason::Timeout));

    fw.core.request_deinit(MagicKind::BtWorker);
    fw.m.worker_blocked = true;
    fw.resume_worker(false);
    assert_eq!(fw.host.wakes.last(), Some(&WakeReason::Deinit));
    assert_eq!(fw.core.outstanding(), 0, "0 outstanding at the end");
}

#[test]
fn an_event_the_worker_has_not_been_woken_for_makes_its_park_not_sleep() {
    // Two events in one poll interval are one wake, and both are drained.
    let mut fw = Firmware::new(WakeMode::U5Polling);
    fw.boot_radio();
    fw.post(1);
    fw.post(2);
    fw.resume_worker(false);
    assert_eq!(fw.host.wakes, [WakeReason::Event]);
    assert_eq!(fw.delivered, [1, 2]);
    assert_eq!(fw.m.wait_ticks, [20, 20]);

    // An event queued before the worker first parks makes the wait poll with 0 ticks, so it
    // returns at once; only the next wait sleeps for the poll interval.
    let mut fw = Firmware::new(WakeMode::U5Polling);
    fw.post(3);
    fw.boot_radio();
    assert_eq!(fw.m.wait_ticks, [0, 20]);
    assert_eq!(fw.host.wakes, [WakeReason::Event]);
    assert_eq!(fw.delivered, [3]);
    assert!(fw.m.worker_blocked);
}

#[test]
fn a_parked_worker_restores_from_a_snapshot_and_wakes_identically() {
    // The worker is parked with an event queued and not yet seen.
    let mut fw = Firmware::new(WakeMode::U5Polling);
    fw.boot_radio();
    fw.post(9);

    let snapshot = fw.core.section.encode().expect("encode");
    let restored = HleSection::decode(&snapshot).expect("decode");
    assert_eq!(restored, fw.core.section);

    // A fresh core registers the worker from binding again; the section is the snapshot.
    let mut fw2 = Firmware::new(WakeMode::U5Polling);
    fw2.core.section = restored;
    fw2.g = fw.g.clone();
    fw2.m = fw.m.clone();

    fw.resume_worker(false);
    fw2.resume_worker(false);
    assert_eq!(fw.host.wakes, fw2.host.wakes);
    assert_eq!(fw2.delivered, [9]);
    assert_eq!(
        fw.hle_bytes(),
        fw2.hle_bytes(),
        "identical state after the restore"
    );
}

#[test]
fn the_worker_wait_is_refused_from_isr_context() {
    // A park in ISR context while the worker is the interrupted task would block inside an ISR:
    // the guard refuses it with E_HLE naming the call, and nothing is left outstanding.
    let mut fw = Firmware::new(WakeMode::U5Polling);
    fw.boot_radio();
    let before = fw.core.outstanding();
    fw.switch_to(Task::Worker);
    fw.g.in_isr = true;
    let top = fw.isr_top;
    fw.g.set_reg(SP, top);
    let hook = HandlerKind(5);
    let mut host = ScriptHost::new(hook, vec![HleAction::Park]);
    let err = fw
        .core
        .on_hook(
            &mut fw.g,
            &mut host,
            TEXT + 0x4000,
            HookRef {
                kind: HookKind::Hle(hook),
                module: ModuleIndex::FIRST_MODULE,
            },
        )
        .expect_err("a blocking wait inside an ISR");
    assert_eq!(err.kind, HleErrorKind::BlockingInIsr, "{err}");
    assert!(err.detail.contains("xQueueSemaphoreTake"), "{err}");
    assert_eq!(fw.core.outstanding(), before);
}

#[test]
fn a_u4_profile_without_an_interrupt_source_is_refused() {
    // Which interrupt source BLE uses is UNVERIFIED: the core never raises one on a guess.
    let fw = Firmware::new(WakeMode::U5Polling);
    let mut core = HleCore::new(fw.core.engine, crate::binding::BoundHooks::default());
    let err = core
        .add_worker(WorkerConfig {
            profile: WorkerProfile {
                wake: WakeMode::U4MagicIsr,
                ..bt_controller_profile()
            },
            calls: CALLS,
        })
        .expect_err("no source");
    assert!(err.detail.contains("no interrupt source"), "{err}");
    let mut fw = Firmware::new(WakeMode::U4MagicIsr);
    fw.boot_radio();
    assert_eq!(fw.m.wait_ticks, [PORT_MAX_DELAY]);
}
