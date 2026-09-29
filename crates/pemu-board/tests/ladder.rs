//! The button ladder model, the curve-fitting inverse the SAR ADC uses, and the `click`,
//! `long_press` and `press` helpers.

use pemu_board::ladder::{
    AdcCal, Atten, ButtonLadder, CLICK_HOLD_MS, LONG_PRESS_HOLD_MS, LadderConfig, RAW_MAX,
    SETTLE_MS, click, curve_mv, inverse_curve, long_press, press,
};
use pemu_core::input::{ButtonId, InputEvent};
use pemu_core::journal::{Journal, Origin};
use pemu_core::time::VTime;

/// The board description's `raw_code` must still come out of the ladder voltages, so a change to
/// one that does not change the other fails here.
#[test]
fn the_four_ladder_codes_come_out_of_the_inverse_curve() {
    let cal = AdcCal::default();
    let cfg = LadderConfig::default();
    assert!(cal.supports_curve_fitting());
    assert_eq!(cal.digi_atten3(), 2_000);

    assert_eq!(inverse_curve(cfg.up_mv, Atten::Db12, cal), 3);
    assert_eq!(inverse_curve(cfg.down_mv, Atten::Db12, cal), 393);
    assert_eq!(inverse_curve(cfg.ok_mv, Atten::Db12, cal), 782);
    // 394, the device's Down code, is unreachable: 393 and 394 both read 274 mV.
    assert_eq!(curve_mv(393, Atten::Db12, cal), 274);
    assert_eq!(curve_mv(394, Atten::Db12, cal), 274);
    assert_eq!(inverse_curve(cfg.released_mv, Atten::Db12, cal), RAW_MAX);

    assert_eq!(cfg.button_raw(ButtonId::Up), 3);
    assert_eq!(cfg.button_raw(ButtonId::Down), 393);
    assert_eq!(cfg.button_raw(ButtonId::Ok), 782);
    assert_eq!(cfg.released_raw(), RAW_MAX);
}

/// The code below each answer must report less than the target, which makes "first" meaningful.
#[test]
fn the_inverse_takes_the_first_code_that_reaches_the_target() {
    let cal = AdcCal::default();
    for (mv, raw) in [(300u32, 433u16), (595, 862)] {
        assert_eq!(curve_mv(raw, Atten::Db12, cal), mv);
        assert!(curve_mv(raw - 1, Atten::Db12, cal) < mv);
    }
}

/// Includes the full-scale row: `raw` 4095 reports 2727 mV, outside every button window.
#[test]
fn the_forward_curve_matches_the_measured_table() {
    let cal = AdcCal::default();
    let table = [
        (0u16, 1u32),
        (107, 75),
        (214, 150),
        (433, 300),
        (644, 447),
        (862, 595),
        (1_459, 1_000),
        (2_798, 1_900),
        (3_726, 2_500),
        (4_095, 2_727),
    ];
    for (raw, mv) in table {
        assert_eq!(curve_mv(raw, Atten::Db12, cal), mv, "raw {raw}");
    }
}

/// The field is 10 bits with bit 9 as the sign: 0x020 is +32 and 0x220 is -32 around 2000.
#[test]
fn a_non_zero_calibration_field_shifts_the_codes_as_the_table_says() {
    let plus = AdcCal {
        blk_version_major: 1,
        cal_vol_atten3: 0x020,
    };
    let minus = AdcCal {
        blk_version_major: 1,
        cal_vol_atten3: 0x220,
    };
    assert_eq!(plus.digi_atten3(), 2_032);
    assert_eq!(minus.digi_atten3(), 1_968);
    assert_eq!(inverse_curve(300, Atten::Db12, plus), 440);
    assert_eq!(inverse_curve(300, Atten::Db12, minus), 426);
    assert_eq!(inverse_curve(595, Atten::Db12, plus), 876);
    assert_eq!(inverse_curve(595, Atten::Db12, minus), 848);
}

/// The model must reproduce the firmware reading 0 mV (UP held forever) rather than calibrating
/// anyway.
#[test]
fn a_blank_calibration_efuse_reports_zero_millivolts() {
    let blank = AdcCal {
        blk_version_major: 0,
        cal_vol_atten3: 0,
    };
    assert!(!blank.supports_curve_fitting());
    assert_eq!(curve_mv(4_095, Atten::Db12, blank), 0);
    // Every target above 0 mV then saturates, because no code ever reaches it.
    assert_eq!(inverse_curve(0, Atten::Db12, blank), 0);
    assert_eq!(inverse_curve(1, Atten::Db12, blank), RAW_MAX);
}

#[test]
fn the_pressed_set_gives_one_voltage_and_the_lowest_wins() {
    let mut ladder = ButtonLadder::default();
    let cal = AdcCal::default();
    assert_eq!(ladder.adc_mv(), 3_300);
    assert_eq!(ladder.raw_code(cal), RAW_MAX);

    assert!(ladder.set(ButtonId::Ok, true));
    assert!(!ladder.set(ButtonId::Ok, true));
    assert_eq!(ladder.adc_mv(), 540);
    assert_eq!(ladder.raw_code(cal), 782);

    ladder.set(ButtonId::Up, true);
    assert_eq!(ladder.adc_mv(), 3);
    assert_eq!(ladder.raw_code(cal), 3);

    ladder.set(ButtonId::Up, false);
    assert_eq!(ladder.adc_mv(), 540);
    ladder.release_all();
    assert_eq!(ladder.adc_mv(), 3_300);
    assert!(!ladder.is_down(ButtonId::Ok));
}

#[test]
fn any_pressed_button_pulls_gpio0_below_the_input_threshold() {
    let cfg = LadderConfig::default();
    for id in ButtonId::ALL {
        let mut ladder = ButtonLadder::default();
        ladder.set(id, true);
        assert!(ladder.reads_low(), "{id:?} should read low");
        assert!(ladder.adc_mv() < cfg.digital_vil_mv);
    }
    assert!(!ButtonLadder::default().reads_low());
}

/// A board whose pull-up is held in deep sleep reads as it does awake.
#[test]
fn gpio0_reads_low_in_deep_sleep_on_this_board() {
    let mut ladder = ButtonLadder::default();
    assert!(!LadderConfig::default().pullup_held_in_deep_sleep);
    assert!(ladder.low_in_deep_sleep(), "released");
    ladder.set(ButtonId::Ok, true);
    assert!(ladder.low_in_deep_sleep(), "pressed");

    let held = LadderConfig {
        pullup_held_in_deep_sleep: true,
        ..LadderConfig::default()
    };
    let mut ladder = ButtonLadder::new(held);
    assert!(!ladder.low_in_deep_sleep(), "released, pull-up held");
    ladder.set(ButtonId::Ok, true);
    assert!(ladder.low_in_deep_sleep(), "pressed, pull-up held");
}

#[test]
fn click_expands_to_two_journal_entries_and_a_run_window() {
    let script = click(ButtonId::Down);
    assert_eq!(script.run_for_ms, CLICK_HOLD_MS + SETTLE_MS);

    let mut journal = Journal::new();
    let now = VTime::from_ms(1_000);
    let until = script.append_to(&mut journal, now, Origin::Agent);
    assert_eq!(until, VTime::from_ms(1_000 + CLICK_HOLD_MS + SETTLE_MS));

    let entries = journal.pending();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].at, now);
    assert_eq!(
        entries[0].ev,
        InputEvent::Button {
            id: ButtonId::Down,
            down: true
        }
    );
    assert_eq!(entries[1].at, VTime::from_ms(1_000 + CLICK_HOLD_MS));
    assert_eq!(
        entries[1].ev,
        InputEvent::Button {
            id: ButtonId::Down,
            down: false
        }
    );
    assert_eq!(entries[0].origin, Origin::Agent);
}

#[test]
fn long_press_clears_the_1500_ms_threshold_and_press_is_raw() {
    let long = long_press(ButtonId::Ok);
    assert_eq!(long.steps[1].at_ms, LONG_PRESS_HOLD_MS);
    const { assert!(LONG_PRESS_HOLD_MS > 1_500) };
    assert_eq!(long.run_for_ms, LONG_PRESS_HOLD_MS + SETTLE_MS);

    let raw = press(ButtonId::Up, 7);
    assert_eq!(raw.steps[0].at_ms, 0);
    assert_eq!(raw.steps[1].at_ms, 7);
    assert_eq!(raw.run_for_ms, 7);
}
