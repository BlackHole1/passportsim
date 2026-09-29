//! `MockBoard` self-tests: the log keeps call order, and the scripted ports answer as the real
//! chips would (an absent I2C address NACKs).

use pemu_board::power::RailState;
use pemu_board::traits::{BoardPorts, PcmFormat};
use pemu_board::usb_plug::UsbHostState;
use pemu_core::time::VTime;
use pemu_testkit::mock_board::{BoardCall, MockBoard};

/// The CW2017 fuel gauge address of the board; any address works here.
const GAUGE: u8 = 0x63;

#[test]
fn t0_the_log_keeps_every_call_in_order() {
    let mut board = MockBoard::new().with_i2c_reads(GAUGE, &[0x12, 0x34]);
    let t = VTime::from_us(100);

    board.spi2(t, false, &[0x2A], false);
    board.spi2(t, true, &[0x00, 0x00, 0x00, 0xEF], true);
    board.i2c_start(t, GAUGE, false);
    board.i2c_write(t, 0x02);
    board.i2c_start(t, GAUGE, true);
    board.i2c_read(t);
    board.i2c_read(t);
    board.i2c_stop(t);
    board.ledc(t, 0, 512, 10, 5000);
    board.i2s_dac(t, PcmFormat::BSP, &[1, -1]);

    assert_eq!(
        board.port_sequence(),
        vec![
            "spi2",
            "spi2",
            "i2c_start",
            "i2c_write",
            "i2c_start",
            "i2c_read",
            "i2c_read",
            "i2c_stop",
            "ledc",
            "i2s_dac",
        ]
    );
    assert_eq!(board.call_count(), 10);
    assert_eq!(board.spi2_bytes(), vec![0x2A, 0x00, 0x00, 0x00, 0xEF]);
    assert!(board.log().iter().all(|c| c.time() == Some(t)));
    assert_eq!(board.calls_to("i2c_read").len(), 2);
}

#[test]
fn t0_an_unscripted_i2c_address_nacks_and_a_scripted_one_reads_its_queue() {
    let mut board = MockBoard::new().with_i2c_reads(GAUGE, &[0xAB]);
    let t = VTime(0);

    assert!(!board.i2c_start(t, 0x18, false), "0x18 is not on this bus");
    assert!(
        !board.i2c_write(t, 0x00),
        "a write after a NACK is not ACKed"
    );
    assert_eq!(board.addressed(), None);
    board.i2c_stop(t);

    assert!(board.i2c_start(t, GAUGE, true));
    assert_eq!(board.addressed(), Some(GAUGE));
    assert_eq!(board.i2c_read(t), 0xAB, "the scripted byte");
    assert_eq!(board.i2c_read(t), 0xFF, "past the queue the bus reads high");
    board.i2c_stop(t);
    assert_eq!(board.addressed(), None);

    assert_eq!(
        board.log()[0],
        BoardCall::I2cStart {
            t,
            addr: 0x18,
            read: false,
            ack: false,
        }
    );
}

#[test]
fn t0_the_reading_ports_answer_the_script_and_default_otherwise() {
    let mut board = MockBoard::new()
        .with_adc_mv(0, 4, 1650)
        .with_gpio_in(9, true)
        .with_mic(&[7, 8, 9])
        .with_rail(RailState::On)
        .with_usb(UsbHostState::U3);
    let t = VTime(0);

    assert_eq!(board.adc_mv(t, 0, 4), 1650);
    assert_eq!(board.adc_mv(t, 0, 5), 0, "an unscripted channel reads 0");
    assert!(board.gpio_in(t, 9));
    assert!(!board.gpio_in(t, 10), "an unscripted pin reads low");
    assert_eq!(board.rail(), RailState::On);
    assert_eq!(board.usb(), UsbHostState::U3);

    let mut frames = [0i16; 4];
    board.i2s_adc(t, PcmFormat::BSP, &mut frames);
    assert_eq!(
        frames,
        [7, 8, 9, 0],
        "past the queue the microphone is silent"
    );
}

/// `adc_mv`, `gpio_in`, `rail` and `usb` take `&self` and are reached through
/// `&mut dyn BoardPorts`, so they must record through the trait itself; driving the mock as a
/// trait object is the check.
#[test]
fn t0_the_and_self_reading_ports_are_recorded_through_the_trait_object() {
    let mut board = MockBoard::new()
        .with_adc_mv(0, 4, 1650)
        .with_gpio_in(9, true);
    let t = VTime::from_ms(2);

    {
        let ports: &mut dyn BoardPorts = &mut board;
        ports.gpio_out(t, 3, true, true);
        assert_eq!(ports.adc_mv(t, 0, 4), 1650);
        assert!(ports.gpio_in(t, 9));
        assert_eq!(ports.rail(), RailState::default());
        assert_eq!(ports.usb(), UsbHostState::default());
    }

    assert_eq!(
        board.port_sequence(),
        vec!["gpio_out", "adc_mv", "gpio_in", "rail", "usb"],
        "a reading port is in the ordered log, after the write that preceded it"
    );
    assert_eq!(
        board.calls_to("adc_mv"),
        vec![BoardCall::AdcMv {
            t,
            unit: 0,
            channel: 4,
            mv: 1650,
        }],
        "the log carries the unit, the channel and the answer"
    );
    assert_eq!(board.log()[3].time(), None, "rail takes no virtual time");
}

#[test]
fn t0_taking_the_log_keeps_the_script() {
    let mut board = MockBoard::new().with_i2c_reads(GAUGE, &[1, 2]);
    let t = VTime(0);
    board.i2c_start(t, GAUGE, true);
    board.i2c_read(t);

    let first = board.take_log();
    assert_eq!(first.len(), 2);
    assert!(board.log().is_empty());

    assert_eq!(board.i2c_read(t), 2, "the read queue kept its cursor");
    assert_eq!(board.call_count(), 1);
}
