//! `Wiring::GpioChanged`: push every output pin that changed to `BoardPorts::gpio_out`
//! (`specs/blocks/gpio.toml`).
//!
//! Only changed pins are reported: the guest rewrites all 26 bits of `GPIO_OUT` to move one, and
//! the LCD path toggles GPIO20 twice per transfer.

use pemu_board::traits::BoardPorts;
use pemu_core::time::VTime;

use crate::periph::gpio;

/// Pushes every changed output pin to the board and returns how many were reported.
///
/// The machine also calls this after every reset, because `Peripheral::reset` raises no wiring
/// effect and the board would otherwise still see the pins the reset dropped.
pub fn apply(model: &mut gpio::Model, board: &mut dyn BoardPorts, now: VTime) -> u32 {
    let changed = model.changed_pins();
    for pin in 0..gpio::PIN_COUNT {
        if changed & (1 << pin) != 0 {
            board.gpio_out(now, pin, model.out_level(pin), model.out_enabled(pin));
        }
    }
    model.mark_published();
    changed.count_ones()
}

/// Samples the board's input levels for the pins in `mask` into `GPIO_IN`.
///
/// Pushed, not pulled: `Peripheral::read` has no board port. The machine calls this when an
/// `InputEvent` changes a pin and after a reset. UNVERIFIED: no firmware on this board polls a
/// GPIO input, so only unit tests cover this path.
pub fn sample_inputs(model: &mut gpio::Model, board: &dyn BoardPorts, now: VTime, mask: u32) {
    for pin in 0..gpio::PIN_COUNT {
        if mask & (1 << pin) != 0 {
            model.set_in_level(pin, board.gpio_in(now, pin));
        }
    }
}

/// Every pin of the block.
pub const ALL_PINS: u32 = (1 << gpio::PIN_COUNT) - 1;

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_board::power::RailState;
    use pemu_board::traits::PcmFormat;
    use pemu_board::usb_plug::UsbHostState;
    use pemu_core::fidelity::FidelityLedger;
    use pemu_core::regstore::Size;

    const T: VTime = VTime(7);

    const OFF_OUT_W1TS: u32 = 0x008;
    const OFF_OUT_W1TC: u32 = 0x00C;
    const OFF_ENABLE_W1TS: u32 = 0x024;

    /// Records the `gpio_out` calls and answers `gpio_in` from a fixed word.
    struct StubBoard {
        out: Vec<(u8, bool, bool)>,
        inputs: u32,
    }

    impl StubBoard {
        fn new() -> Self {
            StubBoard {
                out: Vec::new(),
                inputs: 0,
            }
        }
    }

    impl BoardPorts for StubBoard {
        fn spi2(&mut self, _t: VTime, _dc: bool, _data: &[u8], _cs_release: bool) {}
        fn i2c_start(&mut self, _t: VTime, _addr: u8, _read: bool) -> bool {
            false
        }
        fn i2c_write(&mut self, _t: VTime, _byte: u8) -> bool {
            false
        }
        fn i2c_read(&mut self, _t: VTime) -> u8 {
            0
        }
        fn i2c_stop(&mut self, _t: VTime) {}
        fn i2s_dac(&mut self, _t: VTime, _fmt: PcmFormat, _frames: &[i16]) {}
        fn i2s_adc(&mut self, _t: VTime, _fmt: PcmFormat, _out: &mut [i16]) {}
        fn adc_mv(&self, _t: VTime, _unit: u8, _channel: u8) -> u32 {
            0
        }
        fn gpio_out(&mut self, _t: VTime, pin: u8, level: bool, oe: bool) {
            self.out.push((pin, level, oe));
        }
        fn gpio_in(&self, _t: VTime, pin: u8) -> bool {
            self.inputs & (1 << pin) != 0
        }
        fn ledc(&mut self, _t: VTime, _channel: u8, _duty: u32, _duty_res: u8, _freq_hz: u32) {}
        fn rail(&self) -> RailState {
            RailState::default()
        }
        fn usb(&self) -> UsbHostState {
            UsbHostState::default()
        }
    }

    #[test]
    fn only_changed_pins_reach_the_board() {
        let mut model = gpio::Model::default();
        let mut ledger = FidelityLedger::default();
        let mut board = StubBoard::new();
        let dc = 1u32 << gpio::PIN_LCD_DC;

        model.store(OFF_ENABLE_W1TS, Size::B4, dc, T, &mut ledger);
        model.store(OFF_OUT_W1TS, Size::B4, dc, T, &mut ledger);
        assert_eq!(apply(&mut model, &mut board, T), 1);
        assert_eq!(board.out, vec![(gpio::PIN_LCD_DC, true, true)]);

        // Draining the effect again reports nothing: the board already knows.
        assert_eq!(apply(&mut model, &mut board, T), 0);
        assert_eq!(board.out.len(), 1);

        // The post-callback drops the line; only that pin is reported again.
        model.store(OFF_OUT_W1TC, Size::B4, dc, T, &mut ledger);
        assert_eq!(apply(&mut model, &mut board, T), 1);
        assert_eq!(board.out[1], (gpio::PIN_LCD_DC, false, true));
    }

    #[test]
    fn a_whole_word_write_reports_the_pins_that_moved() {
        let mut model = gpio::Model::default();
        let mut ledger = FidelityLedger::default();
        let mut board = StubBoard::new();

        model.store(0x020, Size::B4, 0b1011, T, &mut ledger);
        model.store(0x004, Size::B4, 0b0011, T, &mut ledger);
        assert_eq!(apply(&mut model, &mut board, T), 3);
        assert_eq!(
            board.out,
            vec![(0, true, true), (1, true, true), (3, false, true)]
        );

        board.out.clear();
        model.store(0x004, Size::B4, 0b1001, T, &mut ledger);
        assert_eq!(apply(&mut model, &mut board, T), 2, "pins 1 and 3 moved");
        assert_eq!(board.out, vec![(1, false, true), (3, true, true)]);
    }

    #[test]
    fn a_reset_is_reported_to_the_board() {
        use pemu_core::reset::{ResetCause, ResetKind};

        let mut model = gpio::Model::default();
        let mut ledger = FidelityLedger::default();
        let mut board = StubBoard::new();

        model.store(0x020, Size::B4, 0b11, T, &mut ledger);
        model.store(0x004, Size::B4, 0b11, T, &mut ledger);
        apply(&mut model, &mut board, T);
        board.out.clear();

        model.reset_to(ResetKind::of(ResetCause::RTC_SW_SYS).expect("a documented cause"));
        assert_eq!(apply(&mut model, &mut board, T), 2);
        assert_eq!(board.out, vec![(0, false, false), (1, false, false)]);
    }

    #[test]
    fn input_levels_come_from_the_board() {
        let mut model = gpio::Model::default();
        let mut ledger = FidelityLedger::default();
        let mut board = StubBoard::new();
        board.inputs = (1 << 9) | (1 << 25);

        sample_inputs(&mut model, &board, T, ALL_PINS);
        assert_eq!(model.in_levels(), (1 << 9) | (1 << 25));
        assert_eq!(
            model.load(0x03C, Size::B4, T, &mut ledger),
            (1 << 9) | (1 << 25)
        );

        board.inputs = 0;
        sample_inputs(&mut model, &board, T, 1 << 9);
        assert_eq!(
            model.in_levels(),
            1 << 25,
            "only the named pin is resampled"
        );
    }
}
