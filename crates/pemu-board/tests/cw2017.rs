//! The CW2017 fuel gauge model.

use pemu_board::battery::{Battery, BatteryConfig, LoadEstimate};
use pemu_board::cw2017::{
    BSP_PROFILE, COMPUTE_WINDOW, CONFIG_ACTIVE, CONFIG_RESTART, CONFIG_SLEEP, ConfigState, Cw2017,
    PROFILE_LEN, REG_CONFIG, REG_SOC_ALERT, REG_SOC_H, REG_TEMP, REG_VCELL_H, REG_VERSION,
    SOC_ALERT_FRESH, SOC_ALERT_PROVISIONED, SOC_COMPUTING, UPDATE_FLAG, VERSION_OVERRIDE,
};
use pemu_board::i2c_transcript::Transcript;
use pemu_board::traits::{BoardDomain, Chip, I2cDevice};
use pemu_core::time::VTime;

const PROFILE_SPEC: &str = include_str!("../../../specs/boards/cw2017-profile.toml");
const INIT_FAST: &str = include_str!("../../../tests/transcripts/i2c/cw2017-init-fast.txt");
const INIT_FRESH: &str = include_str!("../../../tests/transcripts/i2c/cw2017-init-fresh.txt");
const PROBES: &str = include_str!("../../../tests/transcripts/i2c/i2c-boot-probe.txt");

/// A cell with the rail down, so a gauge test is not also a test of the discharge model.
fn cell(soc_milli: u32) -> Battery {
    Battery::new(BatteryConfig::default(), soc_milli)
}

/// Reads through one `i2c_master_transmit_receive`, as the BSP does, so auto-increment is
/// exercised.
fn read_bytes(gauge: &mut Cw2017, t: VTime, reg: u8, count: usize) -> Vec<u8> {
    assert!(gauge.start(t, false));
    assert!(gauge.write(t, reg));
    assert!(gauge.start(t, true));
    let bytes = (0..count).map(|_| gauge.read(t)).collect();
    gauge.stop(t);
    bytes
}

#[test]
fn version_reads_0x0f() {
    // This board's gauge reports 0x0F where the datasheet default is 0xA0.
    for mut gauge in [Cw2017::provisioned(), Cw2017::fresh()] {
        assert_eq!(gauge.reg(REG_VERSION), VERSION_OVERRIDE);
        assert_eq!(read_bytes(&mut gauge, VTime(0), REG_VERSION, 1), [0x0F]);
    }
}

#[test]
fn the_profile_constant_matches_the_spec_file() {
    let body = PROFILE_SPEC
        .split_once("bytes = [")
        .expect("the spec file has a bytes list")
        .1
        .split_once(']')
        .expect("the bytes list is terminated")
        .0;
    let bytes: Vec<u8> = body
        .split(',')
        .map(str::trim)
        .filter(|word| !word.is_empty())
        .map(|word| {
            let digits = word.strip_prefix("0x").unwrap_or(word);
            u8::from_str_radix(digits, 16).unwrap_or_else(|_| panic!("bad byte {word}"))
        })
        .collect();
    assert_eq!(bytes.len(), PROFILE_LEN);
    assert_eq!(bytes, BSP_PROFILE.to_vec());
}

#[test]
fn the_gauge_answers_the_boot_scan() {
    let mut gauge = Cw2017::provisioned();
    Transcript::parse(PROBES)
        .expect("probe transcript parses")
        .replay(&mut gauge, VTime(0))
        .expect("the gauge acknowledges its own address");
}

#[test]
fn the_fast_init_path_replays_against_a_provisioned_gauge() {
    let mut gauge = Cw2017::provisioned();
    let fast = Transcript::parse(INIT_FAST).expect("the fast transcript parses");
    // Match path: probe, VERSION, SOC_ALERT, 80 profile reads, CONFIG.
    assert_eq!(fast.ops.len(), 1 + 1 + 1 + PROFILE_LEN + 1);
    fast.replay(&mut gauge, VTime(0))
        .expect("a provisioned gauge answers every read of the fast path");
    assert!(gauge.profile_matches_bsp());
    assert!(gauge.update_flag());
    assert_eq!(gauge.state(), ConfigState::Computing);
    assert_eq!(gauge.reg(REG_SOC_ALERT), SOC_ALERT_PROVISIONED);
}

#[test]
fn the_slow_init_path_provisions_a_fresh_gauge() {
    let mut gauge = Cw2017::fresh();
    assert!(!gauge.update_flag(), "a fresh gauge has no profile loaded");
    assert!(!gauge.profile_matches_bsp());
    assert_eq!(gauge.reg(REG_SOC_ALERT), SOC_ALERT_FRESH);
    assert_eq!(gauge.reg(REG_CONFIG), CONFIG_SLEEP);
    assert_eq!(gauge.profile(), [0u8; PROFILE_LEN]);

    let fresh = Transcript::parse(INIT_FRESH).expect("the fresh transcript parses");
    // No-match path: probe, VERSION, SOC_ALERT, four CONFIG writes, 80 profile writes, 80
    // read-backs, and the SOC_ALERT read and write that set the update flag.
    let writes = fresh.writes();
    assert_eq!(writes.len(), 4 + PROFILE_LEN + 1);
    let end = fresh
        .replay(&mut gauge, VTime(0))
        .expect("a fresh gauge answers every read-back of the slow path");
    // The CONFIG writes carry 20 ms and 10 ms delays each way (`cw_enter_sleep`,
    // `cw_enter_active`).
    assert_eq!(end, VTime::from_ms(60));

    assert!(gauge.profile_matches_bsp(), "the gauge is now provisioned");
    assert_eq!(gauge.reg(REG_SOC_ALERT), SOC_ALERT_PROVISIONED);
    assert_eq!(gauge.reg(REG_SOC_ALERT) & UPDATE_FLAG, UPDATE_FLAG);
    assert_eq!(gauge.state(), ConfigState::Computing);
    assert_eq!(gauge.profile(), Cw2017::provisioned().profile());
}

#[test]
fn the_config_state_machine_follows_the_datasheet_order() {
    let mut gauge = Cw2017::fresh();
    assert_eq!(gauge.state(), ConfigState::Sleep);
    gauge.write_reg(VTime(0), REG_CONFIG, CONFIG_RESTART);
    assert_eq!(gauge.state(), ConfigState::Restart);
    gauge.write_reg(VTime(0), REG_CONFIG, CONFIG_ACTIVE);
    assert_eq!(gauge.state(), ConfigState::Computing);
    gauge.write_reg(VTime(0), REG_CONFIG, CONFIG_SLEEP);
    assert_eq!(gauge.state(), ConfigState::Sleep);
    assert!(!gauge.soc_ready(), "sleeping loses the computation");
    // The register reads back whatever was written, even a value the state machine ignores.
    gauge.write_reg(VTime(0), REG_CONFIG, 0x11);
    assert_eq!(gauge.reg(REG_CONFIG), 0x11);
    assert_eq!(gauge.state(), ConfigState::Sleep);
}

#[test]
fn soc_reads_out_of_range_until_the_compute_window_closes() {
    let mut gauge = Cw2017::fresh();
    let mut battery = cell(70_000);
    gauge.write_reg(VTime(0), REG_CONFIG, CONFIG_ACTIVE);

    // While computing the gauge may report anything; the BSP rejects values above 100.
    for ms in [0u64, 100, 500, 999] {
        gauge.sample(VTime::from_ms(ms), &mut battery);
        assert_eq!(gauge.reg(REG_SOC_H), SOC_COMPUTING, "at {ms} ms");
        assert!(!gauge.soc_ready());
    }
    gauge.sample(VTime(COMPUTE_WINDOW.0), &mut battery);
    assert!(gauge.soc_ready());
    // The first state of charge after a start is the cell's own, with no lag.
    assert_eq!(gauge.soc_percent(), 70);
    assert!(gauge.reg(REG_SOC_H) <= 100, "the BSP accepts it now");
}

#[test]
fn a_provisioned_gauge_is_settled_before_the_machine_starts() {
    // The provisioned gauge finished computing before this machine existed, so `cw_wait_soc_ready`
    // succeeds on its first read and `bsp_battery_init` finishes at 438 ms, as on the device.
    let gauge = Cw2017::provisioned();
    assert!(
        gauge.soc_ready(),
        "a provisioned gauge is not still computing"
    );
    assert!(
        gauge.reg(REG_SOC_H) <= 100,
        "the BSP rejects {SOC_COMPUTING:#04X}, and it never sees it here"
    );
    assert_ne!(
        gauge.reg(REG_VCELL_H),
        0,
        "the measurement registers describe a cell, not a zeroed snapshot"
    );

    assert!(VTime::from_ms(438) < COMPUTE_WINDOW);
    let mut gauge = Cw2017::provisioned();
    let mut battery = cell(70_000);
    gauge.sample(VTime::from_ms(438), &mut battery);
    assert!(gauge.soc_ready());
    assert_eq!(
        gauge.soc_percent(),
        70,
        "the first sample takes the cell's own value, with no lag to work off"
    );
    assert_eq!(gauge.reg(REG_SOC_H), 70);

    // Only the provisioned start state skips the window: only it was computing before the run
    // began.
    let mut fresh = Cw2017::fresh();
    fresh.write_reg(VTime(0), REG_CONFIG, CONFIG_ACTIVE);
    fresh.sample(VTime::from_ms(438), &mut battery);
    assert!(!fresh.soc_ready());
    assert_eq!(fresh.reg(REG_SOC_H), SOC_COMPUTING);
}

#[test]
fn the_soc_lags_the_cell_over_a_discharge_ramp() {
    let mut gauge = Cw2017::provisioned();
    let mut battery = cell(90_000);
    gauge.sample(VTime(COMPUTE_WINDOW.0), &mut battery);
    assert_eq!(gauge.soc_percent(), 90);
    battery.set_rail_on(true);
    battery.load = LoadEstimate {
        cpu_permille: 1_000,
        backlight_permille: 1_000,
        radio_tx: true,
        speaker_permille: 1_000,
    };

    let mut reported = Vec::new();
    let mut actual = Vec::new();
    for minute in 1..=40u64 {
        let t = VTime(COMPUTE_WINDOW.0 + VTime::from_ms(minute * 60_000).0);
        gauge.sample(t, &mut battery);
        reported.push(gauge.soc_milli());
        actual.push(battery.soc_milli());
    }
    for window in reported.windows(2) {
        assert!(window[1] <= window[0], "the reported value never rises");
    }
    let gaps: Vec<u32> = reported
        .iter()
        .zip(actual.iter())
        .map(|(r, a)| {
            assert!(r >= a, "the lagging value stays above the cell it follows");
            r - a
        })
        .collect();
    assert!(gaps[0] > 0, "the first step lags rather than jumping");
    // Under a steady drain a first-order lag settles to a constant offset rather than catching up.
    let settled = *gaps.last().unwrap();
    assert!(settled >= gaps[0]);
    assert!(
        gaps[gaps.len() - 2].abs_diff(settled) <= 5,
        "the gap has settled: {gaps:?}"
    );
}

#[test]
fn the_soc_alert_is_the_gauge_side_battery_low_derivation() {
    let mut gauge = Cw2017::provisioned();
    // SOC_ALERT 0x94: the 20 percent threshold.
    assert_eq!(gauge.alert_threshold_percent(), 20);

    let mut battery = cell(90_000);
    gauge.sample(VTime(COMPUTE_WINDOW.0), &mut battery);
    assert!(!gauge.soc_alert(), "90 percent is not low");

    battery.set_soc_milli(15_000);
    let mut t = COMPUTE_WINDOW.0;
    for _ in 0..200 {
        t += VTime::from_ms(60_000).0;
        gauge.sample(VTime(t), &mut battery);
    }
    assert_eq!(battery.soc_percent(), 15);
    assert!(
        gauge.soc_alert(),
        "the reported value fell to the threshold, got {}",
        gauge.soc_percent()
    );
}

#[test]
fn vcell_and_temp_convert_the_way_the_datasheet_says() {
    assert_eq!(Cw2017::vcell_raw(3850), 0x3020);
    assert_eq!(Cw2017::vcell_raw(4200), 0x3480);
    assert_eq!(Cw2017::vcell_raw(3300), 0x2940);

    let mut gauge = Cw2017::provisioned();
    let mut battery = Battery::new(BatteryConfig::default(), 100_000);
    battery.set_temp_deci_c(250);
    gauge.sample(VTime(COMPUTE_WINDOW.0), &mut battery);
    let raw = u32::from(gauge.reg(REG_VCELL_H)) << 8 | u32::from(gauge.reg(REG_VCELL_H + 1));
    assert_eq!(raw & 0x3FFF, u32::from(Cw2017::vcell_raw(4200)));
    // The BSP's inverse: `raw * 3125 / 10000` millivolts.
    assert_eq!((raw & 0x3FFF) * 3125 / 10_000, 4200);
    // TEMP is `(T + 40) * 2`, so 25 degrees is 130.
    assert_eq!(gauge.reg(REG_TEMP), 130);
}

#[test]
fn reads_auto_increment_across_a_transaction() {
    let mut gauge = Cw2017::provisioned();
    let mut battery = cell(50_000);
    gauge.sample(VTime(COMPUTE_WINDOW.0), &mut battery);
    // The BSP reads VCELL and SOC as two bytes from one pointer, which needs auto-increment.
    let vcell = read_bytes(&mut gauge, VTime(0), REG_VCELL_H, 2);
    assert_eq!(vcell[0], gauge.reg(REG_VCELL_H));
    assert_eq!(vcell[1], gauge.reg(REG_VCELL_H + 1));
    let soc = read_bytes(&mut gauge, VTime(0), REG_SOC_H, 2);
    assert_eq!(soc[0], gauge.soc_percent());
    assert_eq!(soc[1], gauge.reg(REG_SOC_H + 1));
}

#[test]
fn read_only_registers_ignore_writes() {
    let mut gauge = Cw2017::provisioned();
    for reg in [REG_VERSION, REG_VCELL_H, REG_SOC_H, REG_TEMP] {
        let before = gauge.reg(reg);
        gauge.write_reg(VTime(0), reg, 0x5A);
        assert_eq!(gauge.reg(reg), before, "register {reg:#04X} is read only");
    }
}

#[test]
fn the_gauge_survives_an_mcu_reset_and_not_a_disconnect() {
    let mut gauge = Cw2017::provisioned();
    // The profile and flag survive every MCU domain, keeping `bsp_battery_init` on its fast path.
    for domain in [
        BoardDomain::McuRail,
        BoardDomain::Flash,
        BoardDomain::Host,
        BoardDomain::Card,
    ] {
        gauge.reset(domain);
        assert!(gauge.profile_matches_bsp(), "{domain:?} kept the profile");
    }
    // A battery-domain reset is a disconnect: power-on defaults, the slow path.
    gauge.reset(BoardDomain::Battery);
    assert!(!gauge.update_flag());
    assert_eq!(gauge.reg(REG_CONFIG), CONFIG_SLEEP);
    assert_eq!(gauge.profile(), [0u8; PROFILE_LEN]);
    assert_eq!(
        gauge.reg(REG_VERSION),
        VERSION_OVERRIDE,
        "the part is the same"
    );
}
