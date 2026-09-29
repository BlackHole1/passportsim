//! The power rail transitions, driven through the board so the domains each edge clears are
//! checked at the same time.

use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::power::{PowerConfig, PowerEdge, PowerRail, RailState};
use pemu_board::traits::{BoardCx, BoardDomain, BoardPorts};
use pemu_board::usb_plug::UsbHostState;
use pemu_core::input::InputEvent;
use pemu_core::reset::ResetCause;
use pemu_core::time::VTime;

fn rail_off() -> PowerRail {
    PowerRail::off(PowerConfig::default())
}

#[test]
fn a_500_ms_hold_while_off_raises_the_rail() {
    let mut rail = rail_off();
    assert_eq!(rail.state(), RailState::Off);

    assert_eq!(rail.hold(VTime::from_ms(0), true), None);
    assert_eq!(rail.deadline(), Some(VTime::from_ms(500)));
    assert_eq!(rail.tick(VTime::from_ms(499)), None);
    assert_eq!(rail.state(), RailState::Off);

    assert_eq!(rail.tick(VTime::from_ms(500)), Some(PowerEdge::On));
    assert_eq!(rail.state(), RailState::On);
    // The transition fires once: holding past the deadline does not toggle back.
    assert_eq!(rail.deadline(), None);
    assert_eq!(rail.tick(VTime::from_ms(4_000)), None);
    assert_eq!(rail.state(), RailState::On);
}

/// `power.press({ms})` and a journal replay apply both `Power` entries before virtual time
/// advances, so `tick` never lands inside the press. A model that dropped the hold before
/// evaluating its deadline would leave the rail OFF.
#[test]
fn a_press_released_after_the_deadline_still_raises_the_rail() {
    let mut rail = rail_off();
    assert_eq!(rail.hold(VTime::from_ms(0), true), None);
    assert_eq!(
        rail.hold(VTime::from_ms(600), false),
        Some(PowerEdge::On),
        "a 600 ms press is a 500 ms hold, whatever the scheduler did in between"
    );
    assert_eq!(rail.state(), RailState::On);
    assert_eq!(rail.deadline(), None);
    assert_eq!(rail.tick(VTime::from_ms(10_000)), None);
    assert_eq!(rail.state(), RailState::On);

    let mut on = PowerRail::default();
    on.hold(VTime::from_ms(0), true);
    assert_eq!(on.hold(VTime::from_ms(2_000), false), Some(PowerEdge::Off));
    assert_eq!(on.state(), RailState::Off);
}

#[test]
fn a_press_and_release_with_no_tick_between_them_powers_the_board_on() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    // A new machine starts ON; take the rail down first.
    board.apply_input(
        VTime::from_ms(0),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    board.tick(VTime::from_ms(2_000), &mut cx);
    board.apply_input(
        VTime::from_ms(2_000),
        &InputEvent::Power { down: false },
        &mut cx,
    );
    assert_eq!(board.rail(), RailState::Off);
    cx.take();

    board.apply_input(
        VTime::from_ms(3_000),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let up = board.apply_input(
        VTime::from_ms(3_600),
        &InputEvent::Power { down: false },
        &mut cx,
    );
    assert_eq!(up.rail, Some(PowerEdge::On));
    assert_eq!(board.rail(), RailState::On);
    assert_eq!(
        up.reset.expect("a rail-up edge resets the SoC").cause,
        ResetCause::POWERON
    );
}

/// A machine that drains `BoardCx` and the `BoardEffect` needs no private knowledge of the rail.
#[test]
fn a_power_hold_asks_for_a_wake_up_at_its_deadline() {
    use pemu_board::passport::{BOARD_CHIP, POWER_DEADLINE_TAG};

    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(
        VTime::from_ms(100),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let (timers, _) = cx.take();
    let timer = timers
        .iter()
        .find(|t| t.chip == BOARD_CHIP && t.tag == POWER_DEADLINE_TAG)
        .expect("the board asks for the power deadline");
    // 100 ms plus `off_hold_ms`, because a new machine starts ON.
    assert_eq!(timer.at, VTime::from_ms(2_100));

    let effect = board.tick(timer.at, &mut cx);
    assert_eq!(effect.rail, Some(PowerEdge::Off));
    let (timers, _) = cx.take();
    assert!(
        timers
            .iter()
            .all(|t| !(t.chip == BOARD_CHIP && t.tag == POWER_DEADLINE_TAG)),
        "a consumed hold has no deadline left"
    );
}

#[test]
fn a_short_press_while_off_leaves_the_rail_down() {
    let mut rail = rail_off();
    rail.hold(VTime::from_ms(0), true);
    assert_eq!(rail.hold(VTime::from_ms(200), false), None);
    assert_eq!(rail.state(), RailState::Off);
    assert_eq!(rail.deadline(), None);
    assert_eq!(rail.tick(VTime::from_ms(10_000)), None);
    assert_eq!(rail.state(), RailState::Off);
}

#[test]
fn a_2000_ms_hold_while_on_drops_the_rail_and_a_short_press_does_not() {
    let mut rail = PowerRail::default();
    assert_eq!(rail.state(), RailState::On);

    rail.hold(VTime::from_ms(0), true);
    assert_eq!(rail.hold(VTime::from_ms(1_999), false), None);
    assert_eq!(rail.state(), RailState::On);

    rail.hold(VTime::from_ms(3_000), true);
    assert_eq!(rail.deadline(), Some(VTime::from_ms(5_000)));
    assert_eq!(rail.tick(VTime::from_ms(5_000)), Some(PowerEdge::Off));
    assert_eq!(rail.state(), RailState::Off);
}

#[test]
fn the_cell_below_cutoff_browns_out_only_without_usb_5v() {
    let mut rail = PowerRail::default();
    assert_eq!(rail.check_brownout(2_999, true), None);
    assert_eq!(rail.state(), RailState::On);
    assert_eq!(rail.check_brownout(3_000, false), None);
    assert_eq!(rail.state(), RailState::On);

    assert_eq!(rail.check_brownout(2_999, false), Some(PowerEdge::Brownout));
    assert_eq!(rail.state(), RailState::Off);
    assert_eq!(rail.check_brownout(2_000, false), None);
}

#[test]
fn a_cable_alone_never_powers_the_mcu() {
    let rail = rail_off();
    assert!(!rail.usb_would_power(true));

    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(
        VTime::from_ms(0),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    board.tick(VTime::from_ms(2_000), &mut cx);
    assert_eq!(board.rail(), RailState::Off);
    board.apply_input(
        VTime::from_ms(2_100),
        &InputEvent::UsbCable { plugged: true },
        &mut cx,
    );
    assert_eq!(board.rail(), RailState::Off);
    assert_eq!(board.usb(), UsbHostState::U1);
}

#[test]
fn a_rail_edge_clears_three_domains_and_reports_a_power_on_reset() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));

    board.apply_input(
        VTime::from_ms(0),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let down = board.tick(VTime::from_ms(2_000), &mut cx);
    assert_eq!(down.rail, Some(PowerEdge::Off));
    assert_eq!(
        down.cleared,
        [
            BoardDomain::McuRail,
            BoardDomain::Rtc,
            BoardDomain::BoardRail
        ]
    );
    assert_eq!(down.reset, None);
    let (_, events) = cx.take();
    assert!(events.iter().any(|e| e.name == "power.off"));

    board.apply_input(
        VTime::from_ms(3_000),
        &InputEvent::Power { down: false },
        &mut cx,
    );
    board.apply_input(
        VTime::from_ms(3_100),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let up = board.tick(VTime::from_ms(3_600), &mut cx);
    assert_eq!(up.rail, Some(PowerEdge::On));
    assert_eq!(board.rail(), RailState::On);
    let reset = up.reset.expect("a rail-up edge resets the SoC");
    assert_eq!(reset.cause, ResetCause::POWERON);
    assert_eq!(reset.strap, 0x0A);
}

/// "Hold OK while powering on" is a real recovery gesture; a model that released the ladder at
/// `mcu_rail` would drop it at exactly the boot that reads the button.
#[test]
fn a_button_held_across_a_power_cycle_is_still_held_at_the_next_boot() {
    use pemu_core::input::ButtonId;

    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(
        VTime::from_ms(0),
        &InputEvent::Button {
            id: ButtonId::Ok,
            down: true,
        },
        &mut cx,
    );
    assert_eq!(board.adc_mv(VTime::from_ms(0), 1, 0), 540);

    board.apply_input(
        VTime::from_ms(1),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    board.tick(VTime::from_ms(2_001), &mut cx);
    assert_eq!(board.rail(), RailState::Off);
    assert_eq!(
        board.adc_mv(VTime::from_ms(2_001), 1, 0),
        540,
        "the rail going down does not move a mechanical switch"
    );

    board.apply_input(
        VTime::from_ms(2_100),
        &InputEvent::Power { down: false },
        &mut cx,
    );
    board.apply_input(
        VTime::from_ms(3_000),
        &InputEvent::Power { down: true },
        &mut cx,
    );
    let up = board.tick(VTime::from_ms(3_500), &mut cx);
    assert_eq!(up.rail, Some(PowerEdge::On));
    assert_eq!(board.adc_mv(VTime::from_ms(3_500), 1, 0), 540);
}
