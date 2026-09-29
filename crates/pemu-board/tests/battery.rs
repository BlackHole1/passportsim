//! The cell and the charger. The model is class C, so these tests assert the shape the firmware
//! can observe, not milliamps against a measurement.

use pemu_board::battery::{Battery, BatteryConfig, LoadEstimate, LoadWeights, OCV_MV};
use pemu_board::traits::{BoardDomain, Chip};
use pemu_core::time::VTime;

fn running(soc_milli: u32) -> Battery {
    let mut battery = Battery::new(BatteryConfig::default(), soc_milli);
    battery.set_rail_on(true);
    battery
}

#[test]
fn the_ocv_table_is_monotone_and_ends_at_the_constant_voltage_point() {
    let cfg = BatteryConfig::default();
    for pair in OCV_MV.windows(2) {
        assert!(pair[0] < pair[1], "the curve rises: {pair:?}");
    }
    assert_eq!(u32::from(OCV_MV[OCV_MV.len() - 1]), cfg.cv_mv);
    assert!(
        u32::from(OCV_MV[0]) < cfg.cutoff_mv,
        "an empty cell sits below the cutoff, so the brownout is reachable"
    );

    for (index, &mv) in OCV_MV.iter().enumerate() {
        assert_eq!(cfg.ocv_mv(index as u32 * 10_000), u32::from(mv));
    }
    let mut previous = 0;
    for soc in (0..=100_000).step_by(500) {
        let mv = cfg.ocv_mv(soc);
        assert!(mv >= previous, "the interpolated curve never falls");
        previous = mv;
    }
}

#[test]
fn the_ocv_curve_inverts() {
    let cfg = BatteryConfig::default();
    for soc in (0..=100_000).step_by(1_000) {
        let mv = cfg.ocv_mv(soc);
        let back = cfg.soc_milli_at_ocv(mv);
        // The curve is flat in places: the inverse is exact only to one millivolt on the flattest
        // segment, 10 percent of charge over 40 mV.
        assert!(
            cfg.ocv_mv(back).abs_diff(mv) <= 1,
            "{soc} gave {mv} mV, which inverted to {back}"
        );
    }
    assert_eq!(cfg.soc_milli_at_ocv(0), 0);
    assert_eq!(cfg.soc_milli_at_ocv(5_000), 100_000);
}

#[test]
fn the_load_estimate_sums_its_four_terms() {
    let weights = LoadWeights::default();
    assert_eq!(weights.load_ma(&LoadEstimate::default()), weights.base_ma);
    let all = LoadEstimate {
        cpu_permille: 1_000,
        backlight_permille: 1_000,
        radio_tx: true,
        speaker_permille: 1_000,
    };
    assert_eq!(
        weights.load_ma(&all),
        weights.base_ma
            + weights.cpu_ma
            + weights.backlight_ma
            + weights.radio_tx_ma
            + weights.speaker_ma
    );
    let half = LoadEstimate {
        backlight_permille: 500,
        ..LoadEstimate::default()
    };
    assert_eq!(
        weights.load_ma(&half),
        weights.base_ma + weights.backlight_ma / 2
    );
    let over = LoadEstimate {
        cpu_permille: 5_000,
        ..LoadEstimate::default()
    };
    assert_eq!(weights.load_ma(&over), weights.base_ma + weights.cpu_ma);
}

#[test]
fn the_rail_decides_whether_the_cell_drains() {
    let mut battery = running(50_000);
    assert!(battery.load_ma() > 0);
    battery.advance(VTime::from_ms(600_000));
    let after_ten_minutes = battery.soc_milli();
    assert!(
        after_ten_minutes < 50_000,
        "a running machine drains the cell"
    );

    battery.set_rail_on(false);
    battery.advance(VTime::from_ms(60 * 600_000));
    assert_eq!(battery.soc_milli(), after_ten_minutes);
}

#[test]
fn a_discharge_ramp_is_monotone_and_ends_below_the_cutoff() {
    let mut battery = running(10_000);
    battery.load = LoadEstimate {
        cpu_permille: 1_000,
        backlight_permille: 1_000,
        radio_tx: true,
        speaker_permille: 1_000,
    };
    let mut previous_mv = battery.terminal_mv();
    let mut previous_soc = battery.soc_milli();
    let mut t = 0u64;
    let mut crossed_at = None;
    for _ in 0..600 {
        t += VTime::from_ms(10_000).0;
        battery.advance(VTime(t));
        assert!(battery.soc_milli() <= previous_soc, "the charge only falls");
        assert!(
            battery.terminal_mv() <= previous_mv,
            "the voltage only falls"
        );
        previous_soc = battery.soc_milli();
        previous_mv = battery.terminal_mv();
        if crossed_at.is_none() && battery.below_cutoff() {
            crossed_at = Some(t);
        }
    }
    assert!(
        crossed_at.is_some(),
        "an hour of full load takes a nearly empty 520 mAh cell below the cutoff"
    );
    assert_eq!(battery.soc_milli(), 0, "the cell cannot go below empty");
}

#[test]
fn the_battery_low_derivation_is_the_cutoff_with_no_usb() {
    let cfg = BatteryConfig::default();
    let mut battery = running(0);
    battery.load = LoadEstimate {
        cpu_permille: 1_000,
        backlight_permille: 1_000,
        radio_tx: true,
        speaker_permille: 1_000,
    };
    assert!(battery.terminal_mv() < cfg.cutoff_mv);
    assert!(battery.below_cutoff());

    // The brownout needs both a voltage below the cutoff and no USB 5 V.
    battery.set_usb_5v(true);
    assert!(!battery.below_cutoff(), "a plugged cable is not a brownout");
    battery.set_usb_5v(false);
    assert!(battery.below_cutoff());

    battery.set_soc_milli(50_000);
    assert!(battery.terminal_mv() > cfg.cutoff_mv);
    assert!(!battery.below_cutoff());
}

#[test]
fn the_charger_holds_its_current_then_tapers_and_terminates() {
    let cfg = BatteryConfig::default();
    let mut battery = Battery::new(cfg, 20_000);
    battery.set_usb_5v(true);
    assert_eq!(
        battery.charge_ma(),
        cfg.charge_ma,
        "a half-empty cell takes the constant current"
    );
    assert!(battery.net_ma() > 0, "the cell fills with the rail down");

    let mut currents = Vec::new();
    let mut t = 0u64;
    for _ in 0..600 {
        t += VTime::from_ms(30_000).0;
        battery.advance(VTime(t));
        currents.push(battery.charge_ma());
    }
    for pair in currents.windows(2) {
        assert!(pair[1] <= pair[0], "the charge current only falls");
    }
    assert_eq!(currents.last(), Some(&0));
    assert!(battery.charge_done(), "termination is a latched state");
    assert!(
        battery.soc_percent() >= 99,
        "a terminated charge leaves a full cell, got {}",
        battery.soc_percent()
    );
    // There is no charge-status register: the firmware sees only a risen voltage.
    assert!(battery.terminal_mv() >= cfg.cv_mv - 10);

    battery.set_usb_5v(false);
    assert!(!battery.charge_done());
}

#[test]
fn charging_outruns_an_ordinary_load_while_the_machine_runs() {
    let mut battery = running(30_000);
    battery.load = LoadEstimate {
        cpu_permille: 1_000,
        backlight_permille: 1_000,
        ..LoadEstimate::default()
    };
    let before = battery.soc_milli();
    battery.set_usb_5v(true);
    assert!(battery.net_ma() > 0, "200 mA in beats the modeled load out");
    battery.advance(VTime::from_ms(600_000));
    assert!(battery.soc_milli() > before);
}

#[test]
fn a_constant_load_integrates_the_same_at_every_step_size() {
    let ramp = |steps: u64| {
        let mut battery = running(60_000);
        battery.load = LoadEstimate {
            backlight_permille: 500,
            ..LoadEstimate::default()
        };
        let total = VTime::from_ms(600_000).0;
        for step in 1..=steps {
            battery.advance(VTime(total * step / steps));
        }
        battery.soc_milli()
    };
    let coarse = ramp(1);
    // While the net current is constant the rectangles are exact; step sizes differ only in the
    // microsecond remainder the model carries over.
    for steps in [2u64, 10, 600, 6_000] {
        assert_eq!(ramp(steps), coarse, "{steps} steps");
    }
}

#[test]
fn the_charge_taper_is_the_documented_step_size_exception() {
    // Deep in the CV taper `charge_ma` falls as the cell fills, so the step size changes the
    // answer.
    let charge = |steps: u64| {
        let mut battery = Battery::new(BatteryConfig::default(), 98_000);
        battery.set_usb_5v(true);
        assert!(
            battery.charge_ma() < BatteryConfig::default().charge_ma,
            "the cell starts inside the taper, not at the constant current"
        );
        let total = VTime::from_ms(120_000).0;
        for step in 1..=steps {
            battery.advance(VTime(total * step / steps));
        }
        battery.soc_milli()
    };
    let one_step = charge(1);
    let sixty_steps = charge(60);
    assert!(
        one_step > sixty_steps,
        "one rectangle over the whole two minutes overestimates a falling current: {one_step} \
         against {sixty_steps}"
    );
    // What the firmware can observe is unchanged: the cell fills either way.
    for steps in [1u64, 60, 1_200] {
        assert!(
            charge(steps) > 98_000,
            "{steps} steps still charge the cell"
        );
    }
}

#[test]
fn setting_a_terminal_voltage_inverts_the_curve_at_the_present_load() {
    let mut battery = running(50_000);
    battery.load = LoadEstimate {
        backlight_permille: 1_000,
        ..LoadEstimate::default()
    };
    battery.set_terminal_mv(3800);
    assert!(
        battery.terminal_mv().abs_diff(3800) <= 2,
        "got {} mV",
        battery.terminal_mv()
    );
    let loaded_soc = battery.soc_milli();
    battery.set_rail_on(false);
    battery.set_terminal_mv(3800);
    assert!(battery.soc_milli() < loaded_soc);

    // With the charger running the OCV sits below the terminal voltage, so the inverse subtracts: a
    // host that writes a voltage reads it back.
    battery.set_usb_5v(true);
    assert!(
        battery.net_ma() > 0,
        "the rail is down, so only the charger runs"
    );
    battery.set_terminal_mv(3800);
    assert!(
        battery.terminal_mv().abs_diff(3800) <= 2,
        "charging read back {} mV",
        battery.terminal_mv()
    );
    let charging_soc = battery.soc_milli();
    battery.set_usb_5v(false);
    battery.set_terminal_mv(3800);
    assert!(
        battery.soc_milli() > charging_soc,
        "the same terminal voltage means more charge once the charger stops pushing"
    );
}

#[test]
fn the_temperature_is_host_input_only() {
    let mut battery = Battery::default();
    assert_eq!(battery.temp_deci_c(), Battery::ROOM_TEMP_DECI_C);
    battery.set_temp_deci_c(-105);
    assert_eq!(battery.temp_deci_c(), -105);
    battery.advance(VTime::from_ms(600_000));
    assert_eq!(battery.temp_deci_c(), -105, "nothing in the model moves it");
}

#[test]
fn only_a_disconnect_resets_the_cell() {
    let mut battery = running(30_000);
    for domain in [
        BoardDomain::McuRail,
        BoardDomain::Flash,
        BoardDomain::Host,
        BoardDomain::Card,
    ] {
        battery.reset(domain);
        assert_eq!(battery.soc_milli(), 30_000, "{domain:?} kept the charge");
    }
    battery.reset(BoardDomain::Battery);
    assert_eq!(battery.soc_milli(), Battery::DEFAULT_SOC_MILLI);
    assert!(!battery.rail_on(), "a reconnected cell powers nothing yet");
}
