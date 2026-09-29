//! The power rail state machine: whether the MCU is powered at all.
//!
//! The power button has no GPIO, so it reaches the guest only by switching the rail. A rail-up
//! edge shows the SoC a power-on reset (ESP32-C3 TRM, Reset and Clock chapter). Whether the real
//! rail switches at the N ms mark or on release is UNVERIFIED; this model acts at the mark, so
//! [`PowerRail::tick`] lets the machine schedule the edge independent of the release.

use pemu_core::time::VTime;
use serde::{Deserialize, Serialize};

use crate::traits::BoardDomain;

/// Power rail state returned by `BoardPorts::rail`: what the SoC observes of the power path.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum RailState {
    /// Rail down: SRAM and RTC RAM lost; flash, gauge, card and cable kept.
    #[default]
    Off,
    On,
}

impl RailState {
    pub const fn is_on(self) -> bool {
        matches!(self, RailState::On)
    }
}

/// Why the rail changed, as reported in the event stream and the receipt.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PowerEdge {
    /// OFF to ON after a hold of at least `on_hold_ms`. The SoC sees reset cause 0x01.
    On,
    Off,
    /// ON to OFF because the cell fell below `cutoff_mv` with no USB 5 V.
    Brownout,
}

impl PowerEdge {
    pub const fn name(self) -> &'static str {
        match self {
            PowerEdge::On => "power.on",
            PowerEdge::Off => "power.off",
            PowerEdge::Brownout => "power.brownout",
        }
    }

    /// The domains this edge clears. Rail-up clears them too, since SRAM and RTC RAM start
    /// cleared at power-on.
    pub const fn clears(self) -> &'static [BoardDomain] {
        &[
            BoardDomain::McuRail,
            BoardDomain::Rtc,
            BoardDomain::BoardRail,
        ]
    }
}

/// The `[power]` block of the board file. All values UNVERIFIED: the hold times are from the
/// user instructions ("hold 0.5 s to start, about 2 s to shut down"), `cutoff_mv` is class C.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PowerConfig {
    pub on_hold_ms: u64,
    pub off_hold_ms: u64,
    /// Whether USB 5 V alone powers the MCU. False: the flashing instructions say to power on
    /// first (inference, UNVERIFIED).
    pub usb_powers_mcu: bool,
    /// Cell voltage below which the rail collapses with no USB 5 V, in millivolts.
    pub cutoff_mv: u32,
}

impl Default for PowerConfig {
    fn default() -> Self {
        PowerConfig {
            on_hold_ms: 500,
            off_hold_ms: 2_000,
            usb_powers_mcu: false,
            cutoff_mv: 3_000,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PowerRail {
    cfg: PowerConfig,
    state: RailState,
    held_since: Option<VTime>,
    /// Set once a hold has fired its transition, so one long hold does not toggle twice.
    hold_consumed: bool,
}

impl Default for PowerRail {
    fn default() -> Self {
        PowerRail::new(PowerConfig::default())
    }
}

impl PowerRail {
    /// A rail in its start state: ON, as a new machine starts.
    pub fn new(cfg: PowerConfig) -> Self {
        PowerRail {
            cfg,
            state: RailState::On,
            held_since: None,
            hold_consumed: false,
        }
    }

    /// A rail that starts down, for a scenario that watches the power-on itself.
    pub fn off(cfg: PowerConfig) -> Self {
        PowerRail {
            cfg,
            state: RailState::Off,
            held_since: None,
            hold_consumed: false,
        }
    }

    pub fn config(&self) -> &PowerConfig {
        &self.cfg
    }

    pub fn state(&self) -> RailState {
        self.state
    }

    pub fn is_held(&self) -> bool {
        self.held_since.is_some()
    }

    /// When the current hold would act; `None` when released or already fired.
    pub fn deadline(&self) -> Option<VTime> {
        if self.hold_consumed {
            return None;
        }
        let since = self.held_since?;
        let need = match self.state {
            RailState::Off => self.cfg.on_hold_ms,
            RailState::On => self.cfg.off_hold_ms,
        };
        Some(VTime(since.0.saturating_add(VTime::from_ms(need).0)))
    }

    /// The power button went down or up at `t`. A release after the deadline still fires, because
    /// the deadline is checked before the hold drops: `power.press` and a journal replay need not
    /// land a [`PowerRail::tick`] inside the press.
    pub fn hold(&mut self, t: VTime, down: bool) -> Option<PowerEdge> {
        if down {
            if self.held_since.is_none() {
                self.held_since = Some(t);
                self.hold_consumed = false;
            }
            self.tick(t)
        } else {
            let edge = self.tick(t);
            self.held_since = None;
            self.hold_consumed = false;
            edge
        }
    }

    /// Lets virtual time reach `t` and fires the hold's transition if its deadline has passed.
    pub fn tick(&mut self, t: VTime) -> Option<PowerEdge> {
        let deadline = self.deadline()?;
        if t < deadline {
            return None;
        }
        self.hold_consumed = true;
        Some(match self.state {
            RailState::Off => {
                self.state = RailState::On;
                PowerEdge::On
            }
            RailState::On => {
                self.state = RailState::Off;
                PowerEdge::Off
            }
        })
    }

    /// Re-evaluates the brownout condition: ON, cell below `cutoff_mv`, no USB 5 V. The SoC's own
    /// `RTC_CNTL` brownout detector may reset the CPU first; this is the rail collapsing.
    pub fn check_brownout(&mut self, cell_mv: u32, usb_5v: bool) -> Option<PowerEdge> {
        if self.state.is_on() && cell_mv < self.cfg.cutoff_mv && !usb_5v {
            self.state = RailState::Off;
            self.held_since = None;
            self.hold_consumed = false;
            return Some(PowerEdge::Brownout);
        }
        None
    }

    /// Whether the rail would be up with this cable state. With `usb_powers_mcu = false` a plugged
    /// cable only charges.
    pub fn usb_would_power(&self, cable_plugged: bool) -> bool {
        self.cfg.usb_powers_mcu && cable_plugged
    }
}
