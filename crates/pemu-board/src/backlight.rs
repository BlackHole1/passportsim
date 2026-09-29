//! The LCD backlight: LEDC low-speed channel 0 on GPIO21, duty latched on `para_up`, brightness
//! as an integer pair feeding `FramePort` metadata and the `perceived` view.
//!
//! Brightness is `sig_out_en ? (DUTY_R >> 4) / 2^duty_res : idle_lv` (ESP32-C3 TRM, LED PWM
//! Controller chapter); the duty register carries four fractional bits. The LEDC model shifts
//! them out before calling the board, so `BoardPorts::ledc` delivers the integer duty. Polarity
//! is active high on the device; the duty-to-light curve is not measured. Integer only.

use pemu_core::sched::ChipId;
use pemu_core::time::VTime;
use serde::{Deserialize, Serialize};

use crate::traits::{BoardDomain, Chip};

pub const BACKLIGHT_CHANNEL: u8 = 0;

pub const BACKLIGHT_GPIO: u8 = 21;

/// Duty resolution the BSP configures, in bits (`LEDC_TIMER_10_BIT`).
pub const DEFAULT_DUTY_RES: u8 = 10;

/// Fractional bits of the LEDC duty register.
pub const DUTY_FRACTION_BITS: u32 = 4;

/// Duty resolution [`Brightness::frame_duty`] reports on, in bits. It matches the wasm ABI's
/// `frame_backlight_scale` of `1 << 10`.
pub const FRAME_DUTY_RES: u8 = DEFAULT_DUTY_RES;

/// Brightness as the integer pair `level / scale` (`level = duty >> 4`, `scale = 1 << duty_res`),
/// `level` clamped to `scale`. The arithmetic cannot panic even on a restored pair outside that.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Brightness {
    level: u32,
    /// Never 0.
    scale: u32,
}

impl Brightness {
    /// Dark, on the BSP's 10-bit scale.
    pub const OFF: Brightness = Brightness {
        level: 0,
        scale: 1 << DEFAULT_DUTY_RES,
    };

    /// The pair for a raw LEDC duty register value. A `duty_res` of 0 or above 20 (the width of the
    /// C3 duty field) is clamped so the denominator stays a usable power of two.
    pub fn new(duty_register: u32, duty_res: u8) -> Brightness {
        Brightness::from_duty(duty_register >> DUTY_FRACTION_BITS, duty_res)
    }

    /// The pair for an integer duty (`DUTY_R >> 4`), which is what `BoardPorts::ledc` carries.
    pub fn from_duty(duty: u32, duty_res: u8) -> Brightness {
        let res = duty_res.clamp(1, 20);
        let scale = 1u32 << res;
        Brightness {
            level: duty.min(scale),
            scale,
        }
    }

    pub fn level(self) -> u32 {
        self.level
    }

    pub fn scale(self) -> u32 {
        self.scale
    }

    /// The pair with the polarity applied: an active-low backlight is bright where the duty is low.
    /// Saturates, so a pair outside `level <= scale` reads as dark rather than panicking.
    pub fn with_polarity(self, active_high: bool) -> Brightness {
        if active_high {
            self
        } else {
            Brightness {
                level: self.scale.saturating_sub(self.level),
                scale: self.scale,
            }
        }
    }

    /// This pair as the `FramePort::backlight` duty, renormalised onto [`FRAME_DUTY_RES`] (not
    /// clamped, so a changed `duty_res` stays correct) and shifted up by the four fractional bits.
    pub fn frame_duty(self) -> u16 {
        if self.scale == 0 {
            return 0;
        }
        let full = u64::from(1u32 << FRAME_DUTY_RES) << DUTY_FRACTION_BITS;
        let duty = u64::from(self.level) * full / u64::from(self.scale);
        duty.min(u64::from(u16::MAX)) as u16
    }

    /// True when the backlight emits nothing, so the `perceived` view is black.
    pub fn is_off(self) -> bool {
        self.level == 0
    }

    /// `value` scaled by this brightness, rounding down; dims one RGB565 channel.
    pub fn scale_value(self, value: u32) -> u32 {
        if self.scale == 0 {
            return 0;
        }
        // In `u64`: a pair restored from a snapshot is guest-reachable and must not overflow.
        let scaled = u64::from(value) * u64::from(self.level) / u64::from(self.scale);
        scaled.min(u64::from(u32::MAX)) as u32
    }

    /// Percent of full brightness, rounding down. For receipts and human review only.
    pub fn percent(self) -> u32 {
        if self.scale == 0 {
            return 0;
        }
        (u64::from(self.level) * 100 / u64::from(self.scale)).min(u64::from(u32::MAX)) as u32
    }
}

impl Default for Brightness {
    fn default() -> Self {
        Brightness::OFF
    }
}

/// The LCD backlight chip: the latched LEDC state of one channel. Calls for another channel are
/// ignored.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Backlight {
    channel: u8,
    gpio: u8,
    active_high: bool,
    duty: u32,
    duty_res: u8,
    freq_hz: u32,
    enabled: bool,
    idle_level: bool,
    updates: u64,
}

impl Backlight {
    /// Chip id of the backlight, chosen as the GPIO it drives. UNVERIFIED allocation.
    pub const CHIP_ID: ChipId = ChipId(0x7790);

    /// The backlight as `bsp_display_init` leaves it before `main` raises it: output disabled and
    /// duty 0.
    pub fn new(active_high: bool) -> Backlight {
        Backlight {
            channel: BACKLIGHT_CHANNEL,
            gpio: BACKLIGHT_GPIO,
            active_high,
            duty: 0,
            duty_res: DEFAULT_DUTY_RES,
            freq_hz: 0,
            enabled: false,
            idle_level: false,
            updates: 0,
        }
    }

    pub fn channel(&self) -> u8 {
        self.channel
    }

    pub fn gpio(&self) -> u8 {
        self.gpio
    }

    pub fn active_high(&self) -> bool {
        self.active_high
    }

    /// One latched LEDC channel update (`duty` is `DUTY_R >> 4`); another channel changes nothing.
    /// Enabling follows the duty; [`Backlight::set_enabled`] drives `sig_out_en` directly.
    pub fn ledc(&mut self, t: VTime, channel: u8, duty: u32, duty_res: u8, freq_hz: u32) {
        let _ = t;
        if channel != self.channel {
            return;
        }
        self.duty = duty;
        self.duty_res = duty_res;
        self.freq_hz = freq_hz;
        self.enabled = true;
        self.updates += 1;
    }

    /// Drives `sig_out_en`: with the output disabled the pin rests at `idle_lv`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Sets `idle_lv`, the level the pin rests at while the output is disabled.
    pub fn set_idle_level(&mut self, level: bool) {
        self.idle_level = level;
    }

    /// The latched integer duty, `DUTY_R >> 4`.
    pub fn duty(&self) -> u32 {
        self.duty
    }

    pub fn duty_res(&self) -> u8 {
        self.duty_res
    }

    /// The latched PWM frequency in Hz; no visual effect, kept for receipts.
    pub fn freq_hz(&self) -> u32 {
        self.freq_hz
    }

    /// Whether the channel drives the pin (`sig_out_en`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn updates(&self) -> u64 {
        self.updates
    }

    /// Brightness with `sig_out_en`, `idle_lv` and the polarity applied.
    pub fn brightness(&self) -> Brightness {
        let pair = if self.enabled {
            Brightness::from_duty(self.duty, self.duty_res)
        } else {
            let scale = 1u32 << self.duty_res.clamp(1, 20);
            Brightness {
                level: if self.idle_level { scale } else { 0 },
                scale,
            }
        };
        pair.with_polarity(self.active_high)
    }
}

impl Default for Backlight {
    fn default() -> Self {
        Backlight::new(true)
    }
}

impl Chip for Backlight {
    /// Powered from the MCU rail, so only `mcu_rail` clears it.
    fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::McuRail {
            *self = Backlight::new(self.active_high);
        }
    }
}
