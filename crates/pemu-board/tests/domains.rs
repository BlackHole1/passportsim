//! The persistence-domain reset matrix, and the check that the board description and
//! `BoardConfig::default` agree.
//!
//! The matrix checks what an event does not clear, the half a model gets wrong quietly: a power
//! cycle that also wiped the card would pass every other test in this crate.

use pemu_board::ladder::LadderConfig;
use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::power::{PowerConfig, PowerEdge, RailState};
use pemu_board::traits::{BoardDomain, BoardPorts};
use pemu_board::usb_plug::UsbHostState;
use pemu_core::input::ButtonId;
use pemu_core::rng::DetRng;

/// Read at compile time: a core crate does no file I/O.
const BOARD_TOML: &str = include_str!("../../../boards/ai-passport.toml");

#[test]
fn the_domain_list_is_the_documented_table() {
    assert_eq!(BoardDomain::ALL.len(), 7);
    let names: Vec<&str> = BoardDomain::ALL.iter().map(|d| d.name()).collect();
    assert_eq!(
        names,
        [
            "mcu_rail",
            "rtc",
            "board_rail",
            "battery",
            "card",
            "flash",
            "host"
        ]
    );
}

#[test]
fn power_off_and_brownout_clear_the_same_three_domains() {
    let cleared = [
        BoardDomain::McuRail,
        BoardDomain::Rtc,
        BoardDomain::BoardRail,
    ];
    let kept = [
        BoardDomain::Battery,
        BoardDomain::Card,
        BoardDomain::Flash,
        BoardDomain::Host,
    ];
    for edge in [PowerEdge::On, PowerEdge::Off, PowerEdge::Brownout] {
        assert_eq!(edge.clears(), cleared, "{edge:?}");
        for domain in kept {
            assert!(
                !edge.clears().contains(&domain),
                "{edge:?} must not clear {}",
                domain.name()
            );
        }
    }
}

/// Walks all seven domains against a changed card, so a domain that later grows a card reset
/// fails here.
#[test]
fn only_the_card_domain_reaches_the_card() {
    use pemu_board::ntag213::cmd;

    for domain in BoardDomain::ALL {
        let mut board = PassportBoard::with_seed(&BoardConfig::default(), &mut DetRng::new(11));
        board
            .world
            .card
            .command(&[cmd::WRITE, 0x10, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            board.world.card.page(0x10).unwrap(),
            [0xDE, 0xAD, 0xBE, 0xEF]
        );

        board.reset_domain(domain);
        let survived = board.world.card.page(0x10).unwrap() == [0xDE, 0xAD, 0xBE, 0xEF];
        assert_eq!(
            survived,
            domain != BoardDomain::Card,
            "{} must {} the card",
            domain.name(),
            if domain == BoardDomain::Card {
                "clear"
            } else {
                "keep"
            }
        );
    }
}

/// A rail edge leaves the cable plugged, which is why "OFF plus cable is charge only" is
/// reachable.
#[test]
fn only_the_host_domain_reaches_the_cable() {
    for domain in BoardDomain::ALL {
        let mut board = PassportBoard::from_toml(&BoardConfig::default());
        assert_eq!(board.usb(), UsbHostState::U3);
        board.reset_domain(domain);
        let expected = if domain == BoardDomain::Host {
            UsbHostState::U0
        } else {
            UsbHostState::U3
        };
        assert_eq!(board.usb(), expected, "{}", domain.name());
    }
}

/// The ladder is on the board side of the rail and in no domain. A finger on a button while the
/// device powers on is a real gesture; clearing the ladder at `mcu_rail` would drop it at exactly
/// the boot that reads it.
#[test]
fn no_domain_reset_releases_the_ladder() {
    for domain in BoardDomain::ALL {
        let mut board = PassportBoard::from_toml(&BoardConfig::default());
        board.ladder.set(ButtonId::Down, true);
        board.reset_domain(domain);
        assert_eq!(board.ladder.adc_mv(), 274, "{}", domain.name());
    }
}

/// The rail causes domain resets, so a domain reset that moved it would be a loop.
#[test]
fn no_domain_reset_moves_the_rail() {
    for domain in BoardDomain::ALL {
        let mut board = PassportBoard::from_toml(&BoardConfig::default());
        board.reset_domain(domain);
        assert_eq!(board.rail(), RailState::On, "{}", domain.name());
    }
}

fn row(key: &str, value: &str) {
    let want = format!("{key} = {value}");
    assert!(
        BOARD_TOML.lines().any(|line| line.trim() == want),
        "boards/ai-passport.toml has no row `{want}`"
    );
}

/// Two copies of the board table: `config_hash` uses the code's copy, while the file is what a
/// reader believes, so a value changed in one only is silent drift.
#[test]
fn the_board_description_and_the_default_config_agree() {
    let cfg = BoardConfig::default();
    row("xtal_hz", "40_000_000");
    row("slow_clk_hz", "136_000");
    row("strap", "0x0A");
    row("width", "240");
    row("height", "320");
    row("cs_gpio", "1");
    row("dc_gpio", "20");
    row("ledc_channel", "0");
    row("sda_gpio", "10");
    row("scl_gpio", "7");
    row("on_hold_ms", "500");
    row("off_hold_ms", "2000");
    row("usb_powers_mcu", "false");
    row("cutoff_mv", "3000");
    row("capacity_mah", "520");
    row("cv_mv", "4200");
    row("version_reg", "0x0F");
    row("digital_vil_mv", "825");
    row("pullup_held_in_deep_sleep", "false");
    row("download_strap", "0x06");
    row("mv", "{ up = 3, down = 274, ok = 540, released = 3300 }");
    row(
        "raw_code",
        "{ up = 3, down = 393, ok = 782, released = 4095 }",
    );

    assert_eq!(cfg.xtal_hz, 40_000_000);
    assert_eq!(cfg.slow_clk_hz, 136_000);
    assert_eq!(cfg.strap, 0x0A);
    assert_eq!(cfg.display_width, 240);
    assert_eq!(cfg.display_height, 320);
    assert_eq!(cfg.display_cs_gpio, 1);
    assert_eq!(cfg.display_dc_gpio, 20);
    assert_eq!(cfg.backlight_channel, 0);
    assert_eq!(cfg.i2c_sda_gpio, 10);
    assert_eq!(cfg.i2c_scl_gpio, 7);
    assert_eq!(cfg.power, PowerConfig::default());
    assert_eq!(cfg.power.on_hold_ms, 500);
    assert_eq!(cfg.power.off_hold_ms, 2_000);
    assert!(!cfg.power.usb_powers_mcu);
    assert_eq!(cfg.power.cutoff_mv, 3_000);
    assert_eq!(cfg.battery_capacity_mah, 520);
    assert_eq!(cfg.battery_cv_mv, 4_200);
    assert_eq!(cfg.gauge_version_reg, 0x0F);
    assert_eq!(cfg.buttons, LadderConfig::default());
    assert_eq!(cfg.buttons.digital_vil_mv, 825);
    assert!(!cfg.buttons.pullup_held_in_deep_sleep);
    assert_eq!(cfg.buttons.raw_code, [3, 393, 782, 4_095]);
    assert_eq!(cfg.usb.download_strap, 0x06);
    assert_eq!(cfg.flash_size_mb, 8);
    assert_eq!(cfg.flash_jedec, [0x20, 0x40, 0x17]);
    assert_eq!(cfg.codec_addr, 0x18);
    assert_eq!(cfg.gauge_addr, 0x63);
    assert_eq!(cfg.i2s_gpio, [6, 5, 3, 2, 4]);
}

/// `strap = 0x0A` appears once in the file and once in code (`BoardConfig::strap`), which feeds
/// both the power-on and the line reset. A copy inside `UsbConfig` could latch a different value
/// on an esptool reset than on a power-on.
#[test]
fn the_board_description_holds_the_strap_row_once() {
    let straps: Vec<&str> = BOARD_TOML
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("strap = "))
        .collect();
    assert_eq!(straps, ["strap = 0x0A"], "one `[soc] strap` row");

    let cfg = BoardConfig {
        strap: 0x0C,
        ..BoardConfig::default()
    };
    let mut board = PassportBoard::from_toml(&cfg);
    let mut cx = pemu_board::traits::BoardCx::new(pemu_core::time::VTime(0));
    let line = board
        .apply_input(
            pemu_core::time::VTime::from_ms(1),
            &pemu_core::input::InputEvent::UsbLine {
                dtr: false,
                rts: true,
            },
            &mut cx,
        )
        .reset
        .expect("(RTS 1, DTR 0) resets the chip");
    assert_eq!(line.strap, 0x0C);
}

/// `bsp_i2c_scan`'s 112 probes depend on this.
#[test]
fn only_the_two_configured_i2c_addresses_ack() {
    use pemu_core::time::VTime;

    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let t = VTime::from_ms(1);
    for addr in 0x08u8..=0x77 {
        let expected = addr == 0x18 || addr == 0x63;
        assert_eq!(
            board.i2c_start(t, addr, false),
            expected,
            "addr {addr:#04x}"
        );
        board.i2c_stop(t);
    }
}
