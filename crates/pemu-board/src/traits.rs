//! The board chip traits and the three types their frozen signatures name: [`BoardDomain`] (the
//! persistence domains), [`BoardCx`] (what a chip may do while it runs) and [`PcmFormat`] (the I2S
//! frame layout, per the ESP32-C3 TRM I2S Controller chapter). The trait signatures are frozen.

use pemu_core::sched::ChipId;
use pemu_core::time::VTime;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::power::RailState;
use crate::usb_plug::UsbHostState;

/// A board chip: serializable state that resets per board domain.
pub trait Chip: Serialize + DeserializeOwned {
    /// Reset for one board domain.
    fn reset(&mut self, domain: BoardDomain);
}

/// A chip on the I2C bus.
pub trait I2cDevice: Chip {
    /// 7-bit bus address.
    fn address(&self) -> u8;
    /// Start condition addressed to this chip; returns ACK.
    fn start(&mut self, t: VTime, read: bool) -> bool;
    /// One written byte; returns ACK.
    fn write(&mut self, t: VTime, byte: u8) -> bool;
    fn read(&mut self, t: VTime) -> u8;
    fn stop(&mut self, t: VTime);
}

/// A chip on the SPI bus.
pub trait SpiDevice: Chip {
    /// One SPI transfer with the D/C line level and whether CS releases afterwards.
    fn transfer(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool);
}

/// An I2S audio codec.
pub trait I2sCodec: Chip {
    fn dac(&mut self, t: VTime, fmt: PcmFormat, frames: &[i16]);
    fn adc(&mut self, t: VTime, fmt: PcmFormat, out: &mut [i16]);
}

/// What the SoC side (`wiring`) may call. Implemented by `PassportBoard`, and by `MockBoard` in
/// pemu-testkit.
pub trait BoardPorts {
    fn spi2(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool);
    /// I2C start condition; an absent address NACKs.
    fn i2c_start(&mut self, t: VTime, addr: u8, read: bool) -> bool;
    /// I2C written byte; returns ACK.
    fn i2c_write(&mut self, t: VTime, byte: u8) -> bool;
    fn i2c_read(&mut self, t: VTime) -> u8;
    fn i2c_stop(&mut self, t: VTime);
    fn i2s_dac(&mut self, t: VTime, fmt: PcmFormat, frames: &[i16]);
    fn i2s_adc(&mut self, t: VTime, fmt: PcmFormat, out: &mut [i16]);
    /// Millivolts at an ADC unit and channel.
    fn adc_mv(&self, t: VTime, unit: u8, channel: u8) -> u32;
    fn gpio_out(&mut self, t: VTime, pin: u8, level: bool, oe: bool);
    fn gpio_in(&self, t: VTime, pin: u8) -> bool;
    /// LEDC channel duty and frequency. `duty` is the integer duty `DUTY_R >> 4`
    /// (`periph/ledc.rs` `ChannelOut::duty`), so `duty / 2^duty_res` is the fraction.
    fn ledc(&mut self, t: VTime, channel: u8, duty: u32, duty_res: u8, freq_hz: u32);
    fn rail(&self) -> RailState;
    /// USB host state, U0..U3.
    fn usb(&self) -> UsbHostState;
}

/// Board reset domain of `Chip::reset`.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum BoardDomain {
    /// CPU, SRAM, SoC digital peripherals. Cleared by power OFF and brownout.
    McuRail,
    /// RTC FAST/SLOW RAM, STORE registers, RTC timer, eFuse shadow. Cleared per reset cause; power
    /// OFF and brownout clear everything.
    Rtc,
    /// ST7789P3 frame memory, registers and sleep/display state; ES8311 register array. Cleared by
    /// power OFF or brownout only.
    BoardRail,
    /// CW2017 registers and profile, cell charge. Cleared by `battery.disconnect()` and
    /// `battery.fresh()` only.
    Battery,
    /// NTAG213 memory and counters. Cleared by `nfc.reset()` only.
    Card,
    /// The 8 MB flash image plus overlay. Cleared by an explicit erase or reload.
    Flash,
    /// Cable, clients, bridges. Cleared explicitly.
    Host,
}

impl BoardDomain {
    /// Every domain; the reset matrix iterates this, so a new domain is covered automatically.
    pub const ALL: [BoardDomain; 7] = [
        BoardDomain::McuRail,
        BoardDomain::Rtc,
        BoardDomain::BoardRail,
        BoardDomain::Battery,
        BoardDomain::Card,
        BoardDomain::Flash,
        BoardDomain::Host,
    ];

    /// The domain's name, as a receipt reports it.
    pub const fn name(self) -> &'static str {
        match self {
            BoardDomain::McuRail => "mcu_rail",
            BoardDomain::Rtc => "rtc",
            BoardDomain::BoardRail => "board_rail",
            BoardDomain::Battery => "battery",
            BoardDomain::Card => "card",
            BoardDomain::Flash => "flash",
            BoardDomain::Host => "host",
        }
    }
}

/// Context handed to board calls. A chip cannot hold the scheduler or journal (it could not be
/// snapshotted), so it queues wake-ups and events here for the machine to drain.
pub struct BoardCx {
    now: VTime,
    timers: Vec<ChipTimer>,
    events: Vec<BoardEvent>,
}

/// A wake-up the board asked for at `at`. The machine schedules it under `Owner::Chip(chip)` and
/// hands a due `BOARD_CHIP` wake-up to `PassportBoard::tick`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct ChipTimer {
    pub at: VTime,
    pub chip: ChipId,
    pub tag: u16,
}

/// A named board event (`power.on`, `usb.detached`, `nfc.tapped` ...): name and time only, so no
/// identity bytes can reach a receipt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BoardEvent {
    pub at: VTime,
    pub name: &'static str,
}

impl BoardCx {
    pub fn new(now: VTime) -> Self {
        BoardCx {
            now,
            timers: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn now(&self) -> VTime {
        self.now
    }

    /// Asks for a wake-up tagged `tag` for `chip` at `at`, clamped to `now` like
    /// `Scheduler::schedule`, so a chip cannot schedule into the past.
    pub fn wake(&mut self, chip: ChipId, at: VTime, tag: u16) {
        self.timers.push(ChipTimer {
            at: VTime(at.0.max(self.now.0)),
            chip,
            tag,
        });
    }

    pub fn emit(&mut self, name: &'static str) {
        self.events.push(BoardEvent { at: self.now, name });
    }

    pub fn timers(&self) -> &[ChipTimer] {
        &self.timers
    }

    pub fn events(&self) -> &[BoardEvent] {
        &self.events
    }

    /// Takes both queues and leaves the context empty, so it can be reused without re-delivering.
    pub fn take(&mut self) -> (Vec<ChipTimer>, Vec<BoardEvent>) {
        (
            core::mem::take(&mut self.timers),
            core::mem::take(&mut self.events),
        )
    }

    /// Moves the context to a later virtual time; going backwards is clamped.
    pub fn advance_to(&mut self, now: VTime) {
        self.now = VTime(now.0.max(self.now.0));
    }
}

/// PCM format of an I2S transfer, decoded by the I2S0 model from the clock and slot registers.
/// The BSP configures 16000 Hz, 16 bit, 2 slots; the DAC takes the left slot and capture writes
/// the same sample into both.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct PcmFormat {
    /// Sample rate in hertz; 0 means the I2S clock registers are not configured yet.
    pub fs_hz: u32,
    /// Bits per sample: 16 for this board.
    pub bits: u8,
    /// Slots per frame: 2 for the BSP's stereo slot configuration, 1 for mono.
    pub slots: u8,
}

impl PcmFormat {
    /// The format the BSP configures at `bsp_audio_init`: 16000 Hz, 16 bit, stereo.
    pub const BSP: PcmFormat = PcmFormat {
        fs_hz: 16_000,
        bits: 16,
        slots: 2,
    };

    /// Samples in `frames` frames of this format, saturating rather than overflowing.
    pub const fn samples(self, frames: u32) -> u32 {
        frames.saturating_mul(self.slots as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct Dummy {
        hits: u32,
    }

    impl Chip for Dummy {
        fn reset(&mut self, domain: BoardDomain) {
            let _ = domain;
            self.hits = 0;
        }
    }

    #[test]
    fn a_chip_resets_through_the_trait() {
        let mut chip = Dummy { hits: 3 };
        chip.reset(BoardDomain::BoardRail);
        assert_eq!(chip, Dummy { hits: 0 });
    }

    #[test]
    fn every_domain_has_a_variant_and_its_name() {
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
    fn a_wake_in_the_past_is_clamped_to_now() {
        let mut cx = BoardCx::new(VTime::from_ms(10));
        cx.wake(ChipId(1), VTime::from_ms(4), 9);
        cx.wake(ChipId(1), VTime::from_ms(12), 8);
        assert_eq!(
            cx.timers(),
            [
                ChipTimer {
                    at: VTime::from_ms(10),
                    chip: ChipId(1),
                    tag: 9
                },
                ChipTimer {
                    at: VTime::from_ms(12),
                    chip: ChipId(1),
                    tag: 8
                },
            ]
        );
        cx.emit("power.on");
        let (timers, events) = cx.take();
        assert_eq!(timers.len(), 2);
        assert_eq!(events[0].name, "power.on");
        assert_eq!(events[0].at, VTime::from_ms(10));
        assert!(cx.timers().is_empty() && cx.events().is_empty());
        cx.advance_to(VTime::from_ms(5));
        assert_eq!(cx.now(), VTime::from_ms(10));
    }

    #[test]
    fn the_bsp_pcm_format_is_16_khz_16_bit_stereo() {
        assert_eq!(PcmFormat::BSP.fs_hz, 16_000);
        assert_eq!(PcmFormat::BSP.bits, 16);
        assert_eq!(PcmFormat::BSP.slots, 2);
        assert_eq!(PcmFormat::BSP.samples(240), 480);
    }
}
