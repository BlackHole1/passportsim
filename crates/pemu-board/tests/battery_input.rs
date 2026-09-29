//! `InputEvent::Battery` reaches the cell and the gauge through `PassportBoard::apply_input`,
//! observed through `BoardPorts` as the BSP sees it.

use pemu_board::cw2017::{
    CONFIG_SLEEP, Cw2017, REG_CONFIG, REG_SOC_ALERT, REG_SOC_H, REG_TEMP, REG_VCELL_H,
    SOC_ALERT_FRESH,
};
use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::power::PowerEdge;
use pemu_board::traits::{BoardCx, BoardDomain, BoardPorts};
use pemu_core::input::{BatterySet, InputEvent};
use pemu_core::time::VTime;

const GAUGE: u8 = 0x63;

fn read(board: &mut PassportBoard, t: VTime, reg: u8, n: usize) -> Vec<u8> {
    assert!(board.i2c_start(t, GAUGE, false));
    assert!(board.i2c_write(t, reg));
    assert!(board.i2c_start(t, GAUGE, true));
    let bytes = (0..n).map(|_| board.i2c_read(t)).collect();
    board.i2c_stop(t);
    bytes
}

fn apply(
    board: &mut PassportBoard,
    t: VTime,
    set: BatterySet,
) -> pemu_board::passport::BoardEffect {
    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(t, &InputEvent::Battery(set), &mut cx)
}

/// 312.5 uV per LSB, so 3850 mV is 0x3020 at zero current.
#[test]
fn a_voltage_set_reaches_the_gauge_vcell() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t = VTime::from_ms(100);
    let before = read(&mut board, t, REG_VCELL_H, 2);
    let effect = apply(
        &mut board,
        t,
        BatterySet {
            mv: Some(3_850),
            ..BatterySet::default()
        },
    );
    assert_eq!(effect.rail, None, "3850 mV is above the cutoff");
    let after = read(&mut board, t, REG_VCELL_H, 2);
    assert_ne!(before, after, "the set changed VCELL");
    let raw = u16::from_be_bytes([after[0], after[1]]);
    let expected = Cw2017::vcell_raw(board.battery.terminal_mv());
    assert_eq!(raw, expected, "VCELL is the cell's terminal voltage");
    assert!(
        board.battery.terminal_mv().abs_diff(3_850) <= 5,
        "the cell reads back the voltage the host wrote: {} mV",
        board.battery.terminal_mv()
    );
}

#[test]
fn a_soc_override_is_read_back_on_the_next_gauge_read() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t0 = VTime::from_ms(100);
    assert_eq!(
        read(&mut board, t0, REG_SOC_H, 1),
        [100],
        "settled full cell"
    );
    let t1 = VTime::from_ms(200);
    apply(
        &mut board,
        t1,
        BatterySet {
            soc: Some(10),
            ..BatterySet::default()
        },
    );
    assert_eq!(board.battery.soc_percent(), 10);
    assert_eq!(read(&mut board, t1, REG_SOC_H, 1), [10]);
    assert_eq!(read(&mut board, VTime::from_ms(1_200), REG_SOC_H, 1), [10]);
}

/// The gauge is sampled against the old cell at the set's time first, so the set itself lags over
/// zero time: read at 1 ms, set at 60 s, read at 60 s gives what an unset board reads at 60 s.
#[test]
fn a_set_long_after_the_last_read_does_not_jump_the_gauge_soc() {
    let t_read = VTime::from_ms(1);
    let t_set = VTime::from_ms(60_000);

    let mut untouched = PassportBoard::from_toml(&BoardConfig::default());
    read(&mut untouched, t_read, REG_SOC_H, 1);
    let want = read(&mut untouched, t_set, REG_SOC_H, 1);

    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    read(&mut board, t_read, REG_SOC_H, 1);
    apply(
        &mut board,
        t_set,
        BatterySet {
            mv: Some(3_600),
            ..BatterySet::default()
        },
    );
    assert!(board.battery.soc_percent() < 90, "the set moved the cell");
    assert_eq!(read(&mut board, t_set, REG_SOC_H, 1), want);
}

/// Read once a second, as the demo's Battery page does, the SOC must reach the cell's own whole
/// percent: a lag that stops short leaves the page a percent low for good.
#[test]
fn the_soc_catches_up_with_a_voltage_set_read_once_a_second() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t0 = VTime::from_ms(1_000);
    apply(
        &mut board,
        t0,
        BatterySet {
            soc: Some(20),
            ..BatterySet::default()
        },
    );
    assert_eq!(read(&mut board, t0, REG_SOC_H, 1), [20]);
    apply(
        &mut board,
        t0,
        BatterySet {
            mv: Some(3_700),
            ..BatterySet::default()
        },
    );
    let cell = board.battery.soc_percent();
    assert_eq!(cell, 34, "3700 mV is 34 % on the curve");
    let mut last = 0;
    for second in 1..=600u64 {
        let reported = read(
            &mut board,
            VTime::from_ms(1_000 + second * 1_000),
            REG_SOC_H,
            1,
        )[0];
        assert!(
            reported <= cell,
            "second {second}: {reported} passed the cell's {cell}"
        );
        assert!(
            reported >= last,
            "second {second}: {reported} fell from {last}"
        );
        last = reported;
    }
    assert_eq!(last, cell, "ten minutes on, the gauge reports the cell");
}

/// TEMP is `(T + 40) * 2`, so 25.0 C is 130.
#[test]
fn a_temperature_set_reaches_the_gauge_temp() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t = VTime::from_ms(1);
    apply(
        &mut board,
        t,
        BatterySet {
            temp_c_deci: Some(250),
            ..BatterySet::default()
        },
    );
    assert_eq!(read(&mut board, t, REG_TEMP, 1), [130]);
}

#[test]
fn a_disconnect_resets_the_gauge_to_power_on_defaults() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t = VTime::from_ms(1);
    let effect = apply(
        &mut board,
        t,
        BatterySet {
            present: Some(false),
            ..BatterySet::default()
        },
    );
    assert_eq!(effect.cleared, [BoardDomain::Battery]);
    assert_eq!(read(&mut board, t, REG_CONFIG, 1), [CONFIG_SLEEP]);
    assert_eq!(read(&mut board, t, REG_SOC_ALERT, 1), [SOC_ALERT_FRESH]);
    assert!(!board.gauge.profile_matches_bsp());

    // A reconnect keeps what the disconnect left.
    let effect = apply(
        &mut board,
        t,
        BatterySet {
            present: Some(true),
            ..BatterySet::default()
        },
    );
    assert!(effect.cleared.is_empty());
    assert_eq!(read(&mut board, t, REG_SOC_ALERT, 1), [SOC_ALERT_FRESH]);
}

/// The integration time kept is the disconnect's, not 0, so no time is integrated twice.
#[test]
fn a_reconnect_charges_the_cell_like_a_new_board_and_keeps_its_surroundings() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let new_soc = board.battery.soc_milli();
    board.battery.set_usb_5v(true);
    board.battery.set_rail_on(true);
    board.battery.set_temp_deci_c(310);
    let t = VTime::from_ms(5_000);
    apply(
        &mut board,
        t,
        BatterySet {
            soc: Some(20),
            ..BatterySet::default()
        },
    );
    apply(
        &mut board,
        t,
        BatterySet {
            present: Some(false),
            ..BatterySet::default()
        },
    );
    assert_eq!(
        board.battery.soc_milli(),
        new_soc,
        "charged like a new board"
    );
    assert!(board.battery.usb_5v());
    assert!(board.battery.rail_on());
    assert_eq!(board.battery.temp_deci_c(), 310);
    assert_eq!(
        board.battery.updated(),
        t,
        "integrated up to the disconnect, not reset to 0"
    );
}

#[test]
fn a_voltage_below_the_cutoff_without_usb_browns_out() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    board.apply_input(
        VTime::from_ms(1),
        &InputEvent::UsbCable { plugged: false },
        &mut cx,
    );
    let effect = apply(
        &mut board,
        VTime::from_ms(2),
        BatterySet {
            mv: Some(2_500),
            ..BatterySet::default()
        },
    );
    assert_eq!(effect.rail, Some(PowerEdge::Brownout));
    assert!(effect.cleared.contains(&BoardDomain::McuRail));
}
