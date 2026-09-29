//! The HLE half of the five-point U4 (magic ISR) gate, on the synthetic firmware of
//! `crate::rtos_model` (IDF FreeRTOS callee bodies, interrupt entry and exit modelled). The
//! whole-machine gate is `t1_m8_u4_gate_*` in `tests/milestones/m8.rs`.
//!
//! 1. the worker wakes on a raised radio event;
//! 2. the core lowers the radio source level exactly once per event, at the magic ISR return;
//! 3. when `pxHigherPriorityTaskWoken` is set, the interrupt exit switches to the priority-23
//!    worker;
//! 4. a snapshot taken between the nested `xQueueGiveFromISR` return and the magic ISR return
//!    restores to identical state and output;
//! 5. an event raised inside a critical section is delivered after the section ends.

use pemu_core::snap::SnapSection;

use crate::continuation::{HleSection, Resume};
use crate::core::MAGIC_ISR_HANDLER;
use crate::guest_call::{GuestView, HleErrorKind, SP};
use crate::magic::MagicKind;
use crate::rtos_model::{CALLS, Firmware, SEMAPHORE, TEST_SOURCE, Task};
use crate::worker::{WakeMode, WakeReason};

fn booted() -> Firmware {
    let mut fw = Firmware::new(WakeMode::U4MagicIsr);
    fw.boot_radio();
    assert!(fw.m.worker_blocked, "the worker waits on its semaphore");
    fw
}

#[test]
fn gate_1_the_worker_wakes_on_a_raised_event() {
    let mut fw = booted();
    fw.post(7);
    assert!(fw.level(), "the radio source level is raised");
    assert!(fw.service_interrupt());
    assert_eq!(fw.host.wakes, [WakeReason::Event]);
    assert_eq!(fw.delivered, [7], "the worker got the event");
    assert!(fw.m.worker_blocked, "and waits again");
}

#[test]
fn gate_2_the_core_lowers_the_level_once_per_event_at_the_magic_isr_return() {
    let mut fw = booted();
    for tag in [1u16, 2, 3] {
        fw.post(tag);
        assert!(fw.service_interrupt());
    }
    let worker = fw.core.worker(MagicKind::BtWorker).expect("worker");
    assert_eq!(worker.wake.lowered(), 3, "one lowering per event");
    assert!(!fw.level());
    // Raised by the post and lowered by the core at the magic ISR return, once each per event.
    let levels: Vec<bool> = fw.g.raised.iter().map(|(_, level)| *level).collect();
    assert_eq!(levels, [true, false, true, false, true, false]);
    assert!(
        fw.g.raised
            .iter()
            .all(|(source, _)| *source == u32::from(TEST_SOURCE.0))
    );
    assert_eq!(fw.delivered, [1, 2, 3]);
    assert_eq!(
        fw.core.section.isr_nesting,
        Vec::<u64>::new(),
        "every ISR left"
    );
}

#[test]
fn gate_2_an_event_raised_during_the_isr_is_not_swallowed_by_its_return() {
    let mut fw = booted();
    fw.post(1);
    let (task_sp, _) = fw.enter_interrupt(1).expect("taken");
    fw.post(2); // arrives while the magic ISR's give is outstanding
    fw.finish_isr(task_sp);
    assert!(fw.level(), "the second event keeps the level up");
    assert!(
        fw.service_interrupt(),
        "and is delivered by the next interrupt"
    );
    assert_eq!(fw.delivered, [1, 2]);
    assert!(!fw.level());
}

#[test]
fn gate_3_a_woken_higher_priority_worker_runs_at_interrupt_exit() {
    let mut fw = booted();
    fw.post(11);
    let (task_sp, taken) = fw.enter_interrupt(usize::MAX).expect("taken");
    assert_eq!(taken, 3, "entry, the give return, the yield return");
    assert_eq!(
        fw.m.woken_written,
        [1],
        "pxHigherPriorityTaskWoken came back set"
    );
    assert!(
        fw.m.port_switch,
        "the yield-from-ISR call asked for the switch"
    );
    assert!(
        fw.core
            .worker(MagicKind::BtWorker)
            .expect("worker")
            .wake
            .yield_requested()
    );
    assert!(
        fw.delivered.is_empty(),
        "nothing runs before the interrupt exits"
    );
    fw.exit_interrupt(task_sp);
    assert_eq!(
        fw.host.profile.priority, 23,
        "the worker is the priority-23 task"
    );
    assert_eq!(fw.delivered, [11], "the worker ran at interrupt exit");
    assert_eq!(
        fw.m.running,
        Task::Main,
        "and control is back in the app task"
    );

    // With no task blocked on the semaphore there is no switch to ask for, and no yield call.
    fw.m.worker_blocked = false;
    fw.post(12);
    let (task_sp, taken) = fw.enter_interrupt(usize::MAX).expect("taken");
    assert_eq!(taken, 2, "entry and the give return only");
    assert_eq!(fw.m.woken_written, [1, 0]);
    assert!(!fw.m.port_switch);
    fw.exit_interrupt(task_sp);
}

#[test]
fn gate_4_a_snapshot_between_the_give_and_the_isr_return_restores_identically() {
    // The nested xQueueGiveFromISR has returned, the magic ISR has not.
    let mut fw = booted();
    fw.post(21);
    let (task_sp, taken) = fw.enter_interrupt(2).expect("taken");
    assert_eq!(taken, 2, "stopped after the give returned");
    let isr = fw
        .core
        .section
        .continuations
        .iter()
        .find(|(_, c)| c.handler.handler == MAGIC_ISR_HANDLER)
        .map(|(_, c)| c.clone())
        .expect("the magic ISR is suspended in its yield call");
    assert_eq!(isr.func, CALLS.yield_from_isr);
    assert_eq!(
        fw.core.outstanding(),
        2,
        "the worker's wait and the ISR's call"
    );

    // Nothing of the HLE is cloned into the restored firmware: a fresh core registers the worker
    // from binding, and the section is decoded.
    let snapshot = fw.core.section.encode().expect("encode");
    let mut fw2 = Firmware::new(WakeMode::U4MagicIsr);
    fw2.core.section = HleSection::decode(&snapshot).expect("decode");
    assert_eq!(fw2.core.section, fw.core.section);
    fw2.g = fw.g.clone();
    fw2.m = fw.m.clone();
    assert_eq!(
        fw2.hle_bytes(),
        snapshot.bytes,
        "the restore is byte-identical"
    );

    fw.finish_isr(task_sp);
    fw2.finish_isr(task_sp);
    assert_eq!(fw.delivered, [21]);
    assert_eq!(fw2.delivered, fw.delivered, "identical output");
    assert_eq!(fw2.host.wakes, fw.host.wakes);
    assert_eq!(fw2.g.raised, fw.g.raised);
    assert_eq!(fw2.g.reg(SP), fw.g.reg(SP));
    assert_eq!(fw2.hle_bytes(), fw.hle_bytes(), "identical state");
    assert_ne!(fw.hle_bytes(), snapshot.bytes, "and the run did move on");
}

#[test]
fn gate_5_an_event_raised_inside_a_critical_section_is_delivered_when_it_ends() {
    let mut fw = booted();
    fw.critical(|fw| {
        fw.post(31);
        assert!(fw.level(), "the level is raised while masked");
        assert!(!fw.service_interrupt(), "but no interrupt is taken");
        assert!(fw.delivered.is_empty());
    });
    assert!(
        fw.service_interrupt(),
        "the section ended, the interrupt runs"
    );
    assert_eq!(fw.delivered, [31], "the event is delivered, not lost");
    assert_eq!(
        fw.core
            .worker(MagicKind::BtWorker)
            .expect("worker")
            .wake
            .lowered(),
        1
    );
}

#[test]
fn an_interrupt_before_the_worker_started_leaves_the_event_for_its_first_park() {
    // The radio raised before the scheduler ran the worker: the magic ISR has no semaphore to
    // give, returns, and the worker's first wait does not sleep on the queued event.
    let mut fw = Firmware::new(WakeMode::U4MagicIsr);
    fw.post(41);
    let (task_sp, taken) = fw.enter_interrupt(usize::MAX).expect("taken");
    assert_eq!(taken, 1, "no give without a semaphore");
    fw.exit_interrupt(task_sp);
    assert!(!fw.level(), "the level was still lowered once");
    fw.boot_radio();
    assert_eq!(fw.host.wakes, [WakeReason::Event]);
    assert_eq!(fw.delivered, [41]);
    assert_eq!(fw.m.wait_ticks[0], 0, "the first wait polled");
}

#[test]
fn a_nested_magic_isr_does_not_move_the_generation_of_the_outer_one() {
    // A second radio interrupt preempts the first while its give is outstanding. The inner ISR
    // runs to its return; the outer ISR's call still finds its continuation.
    let mut fw = booted();
    fw.post(51);
    let (task_sp, _) = fw.enter_interrupt(1).expect("outer");
    assert_eq!(fw.core.section.isr_nesting.len(), 1);
    let outer_sp = fw.g.reg(SP);
    let outer_ra = fw.g.reg(crate::guest_call::RA);
    let outer_a = (fw.g.reg(10), fw.g.reg(11));

    // The inner interrupt enters on the same ISR stack below the outer frame.
    fw.post(52);
    fw.g.set_reg(SP, outer_sp - 0x40);
    let pc = fw.core.engine.pcs.bt_isr;
    let mut step = fw
        .core
        .on_hook(
            &mut fw.g,
            &mut fw.host,
            pc,
            crate::hooks::HookRef::core(crate::hooks::HookKind::Magic(MagicKind::BtIsr)),
        )
        .expect("inner entry");
    assert_eq!(fw.core.section.isr_nesting.len(), 2);
    while let crate::core::Step::Resume { pc } = step {
        fw.run_callee(pc);
        step = fw
            .core
            .on_magic_return(&mut fw.g, &mut fw.host)
            .expect("inner");
    }
    assert_eq!(
        fw.core.section.isr_nesting.len(),
        1,
        "the inner ISR left its level"
    );

    // Back in the outer ISR, at its outstanding give.
    fw.g.set_reg(SP, outer_sp);
    fw.g.set_reg(crate::guest_call::RA, outer_ra);
    fw.g.set_reg(10, outer_a.0);
    fw.g.set_reg(11, outer_a.1);
    fw.finish_isr(task_sp);
    assert!(fw.core.section.isr_nesting.is_empty());
    assert_eq!(fw.delivered, [51, 52]);
}

#[test]
fn a_blocking_call_inside_the_magic_isr_is_refused() {
    // The ISR's own calls are FromISR calls; a blocking one from the same place is an E_HLE.
    let mut fw = booted();
    fw.g.in_isr = true;
    fw.g.set_reg(SP, fw.isr_top);
    let call = CALLS.park(SEMAPHORE, 20);
    let err = fw
        .core
        .engine
        .prepare(
            &mut fw.g,
            &fw.core.section.continuations,
            fw.core.section.isr_generation,
            &call,
        )
        .expect_err("a blocking wait inside an ISR");
    assert_eq!(err.kind, HleErrorKind::BlockingInIsr, "{err}");
    let _ = Resume::Entry;
}

#[test]
fn deleting_the_worker_drops_its_queued_events_and_lowers_its_source() {
    // A deinit between a raise and its interrupt must not leave the level up for a worker that
    // no longer exists, nor hand its events to the next init's.
    let mut fw = booted();
    fw.critical(|fw| fw.post(9));
    assert!(fw.level(), "raised, not yet taken");
    fw.g.set_reg(crate::guest_call::A0, crate::rtos_model::TCB_WORKER);
    let observed = fw
        .core
        .on_observe(&mut fw.g, crate::observe::ObserveKind::TaskDelete);
    assert_eq!(observed.task, crate::rtos_model::TCB_WORKER);
    let worker = fw
        .core
        .worker(MagicKind::BtWorker)
        .expect("still registered");
    assert_eq!((worker.task, worker.semaphore), (0, 0));
    assert_eq!(worker.wake.queued(), 0, "the queued event is dropped");
    assert!(!fw.level());
    assert_eq!(
        fw.g.raised.last(),
        Some(&(u32::from(TEST_SOURCE.0), false)),
        "the source is lowered on the fabric"
    );
    assert!(!fw.service_interrupt(), "no interrupt is taken for it");
    assert!(fw.delivered.is_empty());
}
