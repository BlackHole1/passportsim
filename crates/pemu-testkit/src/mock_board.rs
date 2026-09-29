//! `MockBoard`: a `BoardPorts` implementation for SoC peripheral tests without the Passport
//! chips, the board half of `RegHarness`.
//!
//! - A **log**: every [`BoardPorts`] call is appended with its virtual time, so a test asserts
//!   what the SoC side did and in which order. Four ports take `&self` (`adc_mv`, `gpio_in`,
//!   `rail`, `usb`), so the log sits behind a [`RefCell`] and a reading port records its call
//!   through the unchanged trait signature.
//! - A **script**: what the reading ports answer. Everything unscripted has a default (a NACK, a
//!   zero, a low pin), so a test scripts only what it is about.
//!
//! Ordered containers only and no host time, so identical runs give identical logs.

use std::cell::{Ref, RefCell};
use std::collections::BTreeMap;

use pemu_board::power::RailState;
use pemu_board::traits::{BoardPorts, PcmFormat};
use pemu_board::usb_plug::UsbHostState;
use pemu_core::time::VTime;

/// One recorded [`BoardPorts`] call. Byte buffers are copied, so an entry stays readable after
/// the caller reuses its buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardCall {
    /// `spi2`: one transfer with the D/C level and whether CS released afterwards.
    Spi2 {
        t: VTime,
        /// D/C line level: false is a command, true is data.
        dc: bool,
        data: Vec<u8>,
        cs_release: bool,
    },
    /// `i2c_start`: start condition to a 7-bit address, with the ACK the mock answered.
    I2cStart {
        t: VTime,
        addr: u8,
        read: bool,
        ack: bool,
    },
    /// `i2c_write`: one byte written, with the ACK the mock answered.
    I2cWrite {
        t: VTime,
        byte: u8,
        /// The ACK the mock returned.
        ack: bool,
    },
    /// `i2c_read`: one byte read, with the byte the mock returned.
    I2cRead {
        t: VTime,
        /// The byte the mock returned.
        byte: u8,
    },
    /// `i2c_stop`: stop condition.
    I2cStop {
        /// Virtual time of the call.
        t: VTime,
    },
    /// `i2s_dac`: frames played out.
    I2sDac {
        t: VTime,
        fmt: PcmFormat,
        frames: Vec<i16>,
    },
    /// `i2s_adc`: frames recorded in, with the samples the mock supplied.
    I2sAdc {
        t: VTime,
        fmt: PcmFormat,
        frames: Vec<i16>,
    },
    /// `adc_mv`: a SARADC reading, with the millivolts the mock returned.
    AdcMv {
        t: VTime,
        unit: u8,
        channel: u8,
        mv: u32,
    },
    /// `gpio_out`: a pin driven.
    GpioOut {
        t: VTime,
        pin: u8,
        level: bool,
        oe: bool,
    },
    /// `gpio_in`: a pin sampled, with the level the mock returned.
    GpioIn {
        t: VTime,
        pin: u8,
        /// The level the mock returned.
        level: bool,
    },
    /// `ledc`: a channel programmed.
    Ledc {
        t: VTime,
        channel: u8,
        duty: u32,
        duty_res: u8,
        freq_hz: u32,
    },
    /// `rail`: the power rail was read, with the state the mock returned.
    Rail {
        /// The state the mock returned.
        state: RailState,
    },
    /// `usb`: the USB host state was read, with the state the mock returned.
    Usb {
        /// The state the mock returned.
        state: UsbHostState,
    },
}

impl BoardCall {
    pub fn port(&self) -> &'static str {
        match self {
            BoardCall::Spi2 { .. } => "spi2",
            BoardCall::I2cStart { .. } => "i2c_start",
            BoardCall::I2cWrite { .. } => "i2c_write",
            BoardCall::I2cRead { .. } => "i2c_read",
            BoardCall::I2cStop { .. } => "i2c_stop",
            BoardCall::I2sDac { .. } => "i2s_dac",
            BoardCall::I2sAdc { .. } => "i2s_adc",
            BoardCall::AdcMv { .. } => "adc_mv",
            BoardCall::GpioOut { .. } => "gpio_out",
            BoardCall::GpioIn { .. } => "gpio_in",
            BoardCall::Ledc { .. } => "ledc",
            BoardCall::Rail { .. } => "rail",
            BoardCall::Usb { .. } => "usb",
        }
    }

    /// The virtual time the call carried; `rail` and `usb` take none.
    pub fn time(&self) -> Option<VTime> {
        match *self {
            BoardCall::Spi2 { t, .. }
            | BoardCall::I2cStart { t, .. }
            | BoardCall::I2cWrite { t, .. }
            | BoardCall::I2cRead { t, .. }
            | BoardCall::I2cStop { t }
            | BoardCall::I2sDac { t, .. }
            | BoardCall::I2sAdc { t, .. }
            | BoardCall::AdcMv { t, .. }
            | BoardCall::GpioOut { t, .. }
            | BoardCall::GpioIn { t, .. }
            | BoardCall::Ledc { t, .. } => Some(t),
            BoardCall::Rail { .. } | BoardCall::Usb { .. } => None,
        }
    }
}

/// A scriptable stand-in for the board behind the SoC ports.
#[derive(Debug, Default)]
pub struct MockBoard {
    /// Behind a `RefCell` so the `&self` ports record too. No borrow is held across a call into
    /// another port, so the cell never panics.
    log: RefCell<Vec<BoardCall>>,
    i2c_ack: Vec<u8>,
    i2c_reads: BTreeMap<u8, Vec<u8>>,
    i2c_read_cursor: BTreeMap<u8, usize>,
    i2c_addressed: Option<u8>,
    adc_mv: BTreeMap<(u8, u8), u32>,
    gpio_in: BTreeMap<u8, bool>,
    mic: Vec<i16>,
    mic_cursor: usize,
    rail: RailState,
    usb: UsbHostState,
}

impl MockBoard {
    /// A board that NACKs every I2C address, reads 0 everywhere and holds the default rail and
    /// USB states.
    pub fn new() -> MockBoard {
        MockBoard::default()
    }

    /// Makes `addr` ACK its start condition; every other address NACKs.
    pub fn with_i2c_device(mut self, addr: u8) -> MockBoard {
        if !self.i2c_ack.contains(&addr) {
            self.i2c_ack.push(addr);
            self.i2c_ack.sort_unstable();
        }
        self
    }

    /// Queues the bytes `i2c_read` returns while `addr` is addressed, in order; past the end it
    /// reads 0xFF, an undriven bus. Scripting bytes for an address also makes it ACK.
    pub fn with_i2c_reads(mut self, addr: u8, bytes: &[u8]) -> MockBoard {
        self = self.with_i2c_device(addr);
        self.i2c_reads.entry(addr).or_default().extend(bytes);
        self
    }

    /// Sets the millivolts `adc_mv` returns for one unit and channel; unscripted ones return 0.
    pub fn with_adc_mv(mut self, unit: u8, channel: u8, mv: u32) -> MockBoard {
        self.adc_mv.insert((unit, channel), mv);
        self
    }

    /// Sets the level `gpio_in` returns for one pin; unscripted pins read low.
    pub fn with_gpio_in(mut self, pin: u8, level: bool) -> MockBoard {
        self.gpio_in.insert(pin, level);
        self
    }

    /// Queues the samples `i2s_adc` fills its buffer with; past the end it fills silence.
    pub fn with_mic(mut self, samples: &[i16]) -> MockBoard {
        self.mic.extend(samples);
        self
    }

    pub fn with_rail(mut self, rail: RailState) -> MockBoard {
        self.rail = rail;
        self
    }

    /// Sets the USB host state `usb` returns, U0 to U3.
    pub fn with_usb(mut self, usb: UsbHostState) -> MockBoard {
        self.usb = usb;
        self
    }

    fn push(&self, call: BoardCall) {
        self.log.borrow_mut().push(call);
    }

    /// Every recorded call, oldest first, as a `Ref` that derefs to `[BoardCall]`. Do not hold it
    /// across a call into the board.
    pub fn log(&self) -> Ref<'_, [BoardCall]> {
        Ref::map(self.log.borrow(), Vec::as_slice)
    }

    /// The recorded calls to one port, oldest first.
    pub fn calls_to(&self, port: &str) -> Vec<BoardCall> {
        self.log
            .borrow()
            .iter()
            .filter(|c| c.port() == port)
            .cloned()
            .collect()
    }

    /// The port name of every recorded call, oldest first: the order a test asserts.
    pub fn port_sequence(&self) -> Vec<&'static str> {
        self.log.borrow().iter().map(BoardCall::port).collect()
    }

    pub fn call_count(&self) -> usize {
        self.log.borrow().len()
    }

    /// Every byte `spi2` was given, concatenated in call order: the panel command stream.
    pub fn spi2_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for call in self.log.borrow().iter() {
            if let BoardCall::Spi2 { data, .. } = call {
                out.extend_from_slice(data);
            }
        }
        out
    }

    /// Takes the log, leaving the mock's script in place, so a test asserts one phase at a time.
    pub fn take_log(&self) -> Vec<BoardCall> {
        core::mem::take(&mut self.log.borrow_mut())
    }

    pub fn clear_log(&self) {
        self.log.borrow_mut().clear();
    }

    /// The address of the device currently addressed by a start condition, if any.
    pub fn addressed(&self) -> Option<u8> {
        self.i2c_addressed
    }
}

/// Every call is logged before its answer is returned, the `&self` reading ports included, so
/// the log holds everything the SoC side saw.
impl BoardPorts for MockBoard {
    fn spi2(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool) {
        self.push(BoardCall::Spi2 {
            t,
            dc,
            data: data.to_vec(),
            cs_release,
        });
    }

    fn i2c_start(&mut self, t: VTime, addr: u8, read: bool) -> bool {
        let ack = self.i2c_ack.contains(&addr);
        self.i2c_addressed = ack.then_some(addr);
        self.push(BoardCall::I2cStart { t, addr, read, ack });
        ack
    }

    fn i2c_write(&mut self, t: VTime, byte: u8) -> bool {
        let ack = self.i2c_addressed.is_some();
        self.push(BoardCall::I2cWrite { t, byte, ack });
        ack
    }

    fn i2c_read(&mut self, t: VTime) -> u8 {
        let byte = match self.i2c_addressed {
            None => 0xFF,
            Some(addr) => {
                let cursor = self.i2c_read_cursor.entry(addr).or_insert(0);
                let byte = self
                    .i2c_reads
                    .get(&addr)
                    .and_then(|bytes| bytes.get(*cursor))
                    .copied()
                    .unwrap_or(0xFF);
                *cursor += 1;
                byte
            }
        };
        self.push(BoardCall::I2cRead { t, byte });
        byte
    }

    fn i2c_stop(&mut self, t: VTime) {
        self.i2c_addressed = None;
        self.push(BoardCall::I2cStop { t });
    }

    fn i2s_dac(&mut self, t: VTime, fmt: PcmFormat, frames: &[i16]) {
        self.push(BoardCall::I2sDac {
            t,
            fmt,
            frames: frames.to_vec(),
        });
    }

    fn i2s_adc(&mut self, t: VTime, fmt: PcmFormat, out: &mut [i16]) {
        for sample in out.iter_mut() {
            *sample = self.mic.get(self.mic_cursor).copied().unwrap_or(0);
            self.mic_cursor += 1;
        }
        self.push(BoardCall::I2sAdc {
            t,
            fmt,
            frames: out.to_vec(),
        });
    }

    fn adc_mv(&self, t: VTime, unit: u8, channel: u8) -> u32 {
        let mv = self.adc_mv.get(&(unit, channel)).copied().unwrap_or(0);
        self.push(BoardCall::AdcMv {
            t,
            unit,
            channel,
            mv,
        });
        mv
    }

    fn gpio_out(&mut self, t: VTime, pin: u8, level: bool, oe: bool) {
        self.push(BoardCall::GpioOut { t, pin, level, oe });
    }

    fn gpio_in(&self, t: VTime, pin: u8) -> bool {
        let level = self.gpio_in.get(&pin).copied().unwrap_or(false);
        self.push(BoardCall::GpioIn { t, pin, level });
        level
    }

    fn ledc(&mut self, t: VTime, channel: u8, duty: u32, duty_res: u8, freq_hz: u32) {
        self.push(BoardCall::Ledc {
            t,
            channel,
            duty,
            duty_res,
            freq_hz,
        });
    }

    fn rail(&self) -> RailState {
        let state = self.rail;
        self.push(BoardCall::Rail { state });
        state
    }

    fn usb(&self) -> UsbHostState {
        let state = self.usb;
        self.push(BoardCall::Usb { state });
        state
    }
}
