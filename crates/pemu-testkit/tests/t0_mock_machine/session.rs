//! The scripted session checked step by step: outcomes, released outputs, journal and call record.

use super::*;

fn ms(n: u64) -> VTime {
    VTime::from_ms(n)
}

/// 160 instructions per microsecond (the script default) over `n` milliseconds.
fn insns_ms(n: u64) -> u64 {
    n * 1_000 * 160
}

fn run_step(reason: StopReason, vt: VTime, insns: u64) -> Step {
    Step::Run {
        reason,
        vt,
        insns,
        ff_insns: 0,
        idle_ps: 0,
    }
}

fn run_call(
    until: Option<VTime>,
    max_insns: Option<u64>,
    reason: StopReason,
    vt: VTime,
    insns: u64,
) -> MockCall {
    MockCall::Run {
        until,
        max_insns,
        stops: StopSet::default(),
        reason,
        vt,
        insns,
    }
}

fn line(cursor: u64, vt: VTime, text: &str) -> SerialLine {
    SerialLine {
        cursor,
        vt,
        text: text.into(),
    }
}

fn event(seq: u64, vt: VTime, name: &str) -> ReleasedEvent {
    ReleasedEvent {
        seq,
        vt,
        name: name.into(),
    }
}

#[test]
fn t0_scripted_session_through_every_machine_api_method() {
    let mut m = script().build();
    assert_eq!(m.pending_outputs(), 9);
    assert_eq!(m.next_output_at(), Some(VTime(0)));
    let steps = drive(&mut m);

    let panic = StopReason::GuestPanic(PanicCapture::default());
    let expected = vec![
        Step::Now(VTime(0)),
        run_step(StopReason::Until, ms(10), insns_ms(10)),
        run_step(StopReason::Matcher(MatcherId(2)), ms(120), insns_ms(110)),
        run_step(StopReason::MaxInsns, ms(121), 160_000),
        Step::Input(Ok(0)),
        Step::Input(Err(InputError {})),
        Step::Input(Ok(1)),
        run_step(StopReason::Matcher(MatcherId(2)), ms(141), insns_ms(20)),
        run_step(StopReason::Matcher(MatcherId(1)), ms(150), insns_ms(9)),
        run_step(panic.clone(), ms(400), insns_ms(250)),
        run_step(StopReason::Until, ms(1000), insns_ms(600)),
        run_step(StopReason::Deadlock, ms(1000), 0),
        run_step(StopReason::Until, ms(1000), 0),
        run_step(StopReason::MaxInsns, ms(1000), 0),
        Step::Now(ms(1000)),
        Step::Io {
            usj_tx: IO_CAPACITY,
            usj_rx: IO_CAPACITY,
            uart0_tx: IO_CAPACITY,
        },
        Step::GuestMem,
        Step::Receipt(Receipt::default()),
        Step::IsTainted(false),
    ];
    assert_eq!(steps, expected);

    // Serial lines keep per-channel cursors.
    assert_eq!(
        m.serial_lines(MockChan::Usj),
        [
            line(0, VTime(0), "ESP-ROM:esp32c3-api1-20210207"),
            line(1, ms(150), "I (150) pk_app: ready"),
        ]
    );
    assert_eq!(
        m.serial_since(MockChan::Usj, 1),
        [line(1, ms(150), "I (150) pk_app: ready")]
    );
    assert!(m.serial_since(MockChan::Usj, 9).is_empty());
    assert_eq!(
        m.serial_lines(MockChan::Uart0),
        [line(0, ms(5), "I (5) boot: ESP-IDF v5.5.3")]
    );

    // Frames: the scripted menu, then the reaction to the button press 20 ms later.
    let gens: Vec<(u64, VTime, u16)> = m
        .frames()
        .iter()
        .map(|f| (f.generation, f.vt, f.frame.pixels[0]))
        .collect();
    assert_eq!(gens, [(1, ms(120), 0x001f), (2, ms(141), 0xffff)]);
    assert_eq!(m.last_frame().map(|f| f.generation), Some(2));
    assert_eq!(m.last_frame().map(|f| f.frame.dirty_rows), Some((0, 320)));

    assert_eq!(
        m.events(),
        [
            event(0, ms(5), "reset"),
            event(1, ms(121), "input"),
            event(2, ms(150), "ui-settled"),
            event(3, ms(200), "input"),
        ]
    );

    let states: Vec<(&str, VTime, &str)> = m
        .states()
        .iter()
        .map(|(k, v)| (k.as_str(), v.vt, v.value.as_str()))
        .collect();
    assert_eq!(
        states,
        [("lifecycle", VTime(0), "boot"), ("ui", ms(141), "display")]
    );
    assert_eq!(m.state("ui").map(|v| v.value.as_str()), Some("display"));
    assert_eq!(m.state("absent"), None);

    // The refused past input is neither journaled nor numbered.
    let press = MockInput::Button {
        id: ButtonId::Ok,
        down: true,
    };
    let release = MockInput::Button {
        id: ButtonId::Ok,
        down: false,
    };
    let ping = MockInput::SerialIn {
        chan: SerialChan(0),
        data: b"ping".to_vec(),
    };
    assert_eq!(
        m.journal(),
        [
            JournalRecord {
                seq: 0,
                vt: ms(121),
                input: press.clone(),
            },
            JournalRecord {
                seq: 1,
                vt: ms(200),
                input: ping.clone(),
            },
        ]
    );
    assert_eq!(m.pending_outputs(), 0);
    assert_eq!(m.next_output_at(), None);

    let until = Some(ms(1000));
    let expected_calls = vec![
        MockCall::Now { vt: VTime(0) },
        run_call(Some(ms(10)), None, StopReason::Until, ms(10), insns_ms(10)),
        run_call(
            None,
            None,
            StopReason::Matcher(MatcherId(2)),
            ms(120),
            insns_ms(110),
        ),
        run_call(None, Some(160_000), StopReason::MaxInsns, ms(121), 160_000),
        MockCall::Input {
            at: At::Now,
            input: press,
            result: Ok(0),
        },
        MockCall::Input {
            at: At::Vt(ms(100)),
            input: release,
            result: Err(InputError {}),
        },
        MockCall::Input {
            at: At::Vt(ms(200)),
            input: ping,
            result: Ok(1),
        },
        run_call(
            until,
            None,
            StopReason::Matcher(MatcherId(2)),
            ms(141),
            insns_ms(20),
        ),
        run_call(
            until,
            None,
            StopReason::Matcher(MatcherId(1)),
            ms(150),
            insns_ms(9),
        ),
        run_call(until, None, panic, ms(400), insns_ms(250)),
        run_call(until, None, StopReason::Until, ms(1000), insns_ms(600)),
        run_call(None, None, StopReason::Deadlock, ms(1000), 0),
        run_call(Some(VTime(0)), None, StopReason::Until, ms(1000), 0),
        run_call(None, Some(0), StopReason::MaxInsns, ms(1000), 0),
        MockCall::Now { vt: ms(1000) },
        MockCall::Io,
        MockCall::GuestMem,
        MockCall::Receipt { vt: ms(1000) },
    ];
    assert_eq!(m.calls(), expected_calls);
    assert_eq!(m.take_calls(), expected_calls);
    assert!(m.calls().is_empty());
}

#[test]
fn t0_zero_rate_never_reaches_max_insns() {
    let mut m = MockScript::new().insns_per_us(0).build();
    let out = m.run(limits(None, Some(5)));
    assert_eq!(
        (out.reason, out.vt, out.insns),
        (StopReason::Deadlock, VTime(0), 0)
    );
}

#[test]
fn t0_matcher_armed_for_one_run_does_not_fire_in_the_next() {
    let mut m = script().build();
    m.disarm_all_matchers();
    assert!(m.armed_matchers().is_empty());

    m.arm_matcher(MatcherId(7), MockMatcher::Event("reset".into()));
    let first = m.run(limits(None, None));
    assert_eq!(
        (first.reason, first.vt),
        (StopReason::Matcher(MatcherId(7)), ms(5))
    );
    assert!(m.disarm_matcher(MatcherId(7)));
    assert!(!m.disarm_matcher(MatcherId(7)));

    // With nothing armed the next run passes the frame and the ready line and stops at the
    // scripted panic.
    let second = m.run(limits(None, None));
    assert_eq!(
        (second.reason, second.vt),
        (StopReason::GuestPanic(PanicCapture::default()), ms(400))
    );

    // Arming an id twice replaces its matcher and keeps one entry.
    m.arm_matcher(MatcherId(7), MockMatcher::Frame);
    m.arm_matcher(MatcherId(7), MockMatcher::Event("late".into()));
    assert_eq!(
        m.armed_matchers(),
        [(MatcherId(7), MockMatcher::Event("late".into()))]
    );
}
