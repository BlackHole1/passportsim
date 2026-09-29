//! `MockMachine` replays a scripted session through every `MachineApi` method; two identical
//! sessions give identical results.

mod session;

use pemu_core::input::{ButtonId, InputEvent, SerialChan};
use pemu_core::time::VTime;
use pemu_machine::MachineApi;
use pemu_machine::machine::{At, InputError, Receipt};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{MatcherId, PanicCapture, StopReason, StopSet};
use pemu_testkit::mock_machine::{
    JournalRecord, MockCall, MockChan, MockFrame, MockInput, MockMachine, MockMatcher, MockScript,
    ReleasedEvent, ReleasedFrame, SerialLine, StateValue, Timeline,
};

const IO_CAPACITY: usize = 256;

/// The scripted session: boot output, a menu frame, a ready line, a panic stop at 400 ms, and
/// reactions to a button press and to every input.
fn script() -> MockScript {
    MockScript::new()
        .outputs(
            Timeline::new()
                .serial(MockChan::Usj, "ESP-ROM:esp32c3-api1-20210207")
                .state("lifecycle", "boot")
                .at_ms(5)
                .serial(MockChan::Uart0, "I (5) boot: ESP-IDF v5.5.3")
                .event("reset")
                .at_ms(120)
                .frame(MockFrame::solid(0x001f))
                .state("ui", "menu"),
        )
        .outputs(
            Timeline::new()
                .at_ms(150)
                .serial(MockChan::Usj, "I (150) pk_app: ready")
                .event("ui-settled")
                .at_ms(400)
                .stop(StopReason::GuestPanic(PanicCapture::default())),
        )
        .matcher(
            MatcherId(1),
            MockMatcher::Serial {
                chan: MockChan::Usj,
                contains: "ready".into(),
            },
        )
        .matcher(MatcherId(2), MockMatcher::Frame)
        .on_input(
            MockInput::Button {
                id: ButtonId::Ok,
                down: true,
            },
            Timeline::new()
                .at_ms(20)
                .frame(MockFrame::solid(0xffff))
                .state("ui", "display"),
        )
        .on_any_input(Timeline::new().event("input"))
        .io_capacity(IO_CAPACITY)
}

fn limits(until: Option<VTime>, max_insns: Option<u64>) -> RunLimits {
    RunLimits {
        until,
        max_insns,
        stops: StopSet::default(),
    }
}

/// What one step of the session returned.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    Now(VTime),
    Run {
        reason: StopReason,
        vt: VTime,
        insns: u64,
        ff_insns: u64,
        idle_ps: u64,
    },
    Input(Result<u64, InputError>),
    Io {
        usj_tx: usize,
        usj_rx: usize,
        uart0_tx: usize,
    },
    GuestMem,
    Receipt(Receipt),
    /// `is_tainted`, which a mock answers from the trait's default.
    IsTainted(bool),
}

fn run(api: &mut dyn MachineApi, until: Option<VTime>, max_insns: Option<u64>) -> Step {
    let out = api.run(limits(until, max_insns));
    Step::Run {
        reason: out.reason,
        vt: out.vt,
        insns: out.insns,
        ff_insns: out.ff_insns,
        idle_ps: out.idle_ps,
    }
}

fn drive(api: &mut dyn MachineApi) -> Vec<Step> {
    let ms = VTime::from_ms;
    let mut steps = vec![Step::Now(api.now())];
    steps.push(run(api, Some(ms(10)), None));
    steps.push(run(api, None, None));
    steps.push(run(api, None, Some(160_000)));
    let press = InputEvent::Button {
        id: ButtonId::Ok,
        down: true,
    };
    steps.push(Step::Input(api.input(At::Now, press)));
    let late = InputEvent::Button {
        id: ButtonId::Ok,
        down: false,
    };
    steps.push(Step::Input(api.input(At::Vt(ms(100)), late)));
    let ping = InputEvent::SerialIn {
        chan: SerialChan(0),
        data: b"ping".to_vec(),
    };
    steps.push(Step::Input(api.input(At::Vt(ms(200)), ping)));
    for _ in 0..4 {
        steps.push(run(api, Some(ms(1000)), None));
    }
    steps.push(run(api, None, None));
    steps.push(run(api, Some(VTime(0)), None));
    steps.push(run(api, None, Some(0)));
    steps.push(Step::Now(api.now()));
    let io = api.io();
    steps.push(Step::Io {
        usj_tx: io.usj_tx.capacity(),
        usj_rx: io.usj_rx.capacity(),
        uart0_tx: io.uart0_tx.capacity(),
    });
    let _view = api.guest_mem();
    steps.push(Step::GuestMem);
    steps.push(Step::Receipt(api.receipt()));
    steps.push(Step::IsTainted(api.is_tainted()));
    steps
}

/// Everything a session produced, for comparing two sessions.
#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    steps: Vec<Step>,
    usj: Vec<SerialLine>,
    uart0: Vec<SerialLine>,
    frames: Vec<ReleasedFrame>,
    events: Vec<ReleasedEvent>,
    states: Vec<(String, StateValue)>,
    journal: Vec<JournalRecord>,
    calls: Vec<MockCall>,
}

fn transcript() -> Transcript {
    let mut m = script().build();
    let steps = drive(&mut m);
    Transcript {
        steps,
        usj: m.serial_lines(MockChan::Usj).to_vec(),
        uart0: m.serial_lines(MockChan::Uart0).to_vec(),
        frames: m.frames().to_vec(),
        events: m.events().to_vec(),
        states: m.states().clone().into_iter().collect(),
        journal: m.journal().to_vec(),
        calls: m.calls(),
    }
}

#[test]
fn t0_two_identical_sessions_give_identical_results() {
    let a = transcript();
    let b = transcript();
    assert_eq!(a, b);
    assert!(!a.calls.is_empty());
}

#[test]
fn t0_mock_is_usable_as_a_trait_object() {
    let mut m: MockMachine = script().build();
    let api: &mut dyn MachineApi = &mut m;
    assert_eq!(api.now(), VTime(0));
}

#[test]
fn t0_mock_snapshots_forks_and_restores_through_the_trait() {
    use pemu_core::snap::{LivePolicy, SnapOpts};
    use pemu_machine::SnapshotMachine;

    let mut boxed: Box<dyn SnapshotMachine + Send> = Box::new(script().build());
    let api: &mut dyn MachineApi = &mut *boxed;
    api.run(limits(Some(VTime::from_ms(10)), None));
    let snap = boxed
        .snapshot(SnapOpts::default())
        .expect("a mock snapshots");
    let (hash, now) = (boxed.state_hash(), boxed.now());

    let mut fork = boxed.fork(LivePolicy::Refuse).expect("a mock forks");
    assert_eq!((fork.state_hash(), fork.now()), (hash, now));
    let api: &mut dyn MachineApi = &mut *fork;
    api.run(limits(Some(VTime::from_ms(200)), None));
    assert_ne!(fork.state_hash(), hash);
    fork.restore(&snap)
        .expect("a fork restores its parent's snapshot");
    assert_eq!((fork.state_hash(), fork.now()), (hash, now));

    // A snapshot the mock did not take is refused and changes nothing.
    let mut again = script().build();
    MachineApi::run(&mut again, limits(Some(VTime::from_ms(300)), None));
    let before = SnapshotMachine::state_hash(&again);
    let bare = pemu_core::snap::Snapshot::new(Default::default());
    assert!(SnapshotMachine::restore(&mut again, &bare).is_err());
    assert_eq!(SnapshotMachine::state_hash(&again), before);
}
