//! The USB cable, the four host states and the RFC 2217 line-state decoder (ESP32-C3 TRM, USB
//! Serial/JTAG chapter; `specs/blocks/usj.toml`).
//!
//! The U-states describe a generic CDC-ACM host: line state reaches the machine only through
//! RFC 2217, so macOS, Windows and a browser drive the same four rows.

use serde::{Deserialize, Serialize};

use crate::power::RailState;

/// USB host state returned by `BoardPorts::usb`, U0..U3.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum UsbHostState {
    /// U0 DETACHED: unplugged. No SOF, FRAME_NUM frozen, the IN FIFO never drains.
    #[default]
    U0,
    /// U1 CHARGE_ONLY: plugged with the MCU rail down. Charging; the host sees nothing.
    U1,
    /// U2 ATTACHED_IDLE: enumerated CDC-ACM with no client. SOF at 1 kHz; IN does not drain.
    U2,
    /// U3 ATTACHED_OPEN: a client holds the port open. SOF; IN drains; OUT fills EP1.
    U3,
}

impl UsbHostState {
    /// Whether the device is enumerated (U2 and U3).
    pub const fn is_attached(self) -> bool {
        matches!(self, UsbHostState::U2 | UsbHostState::U3)
    }

    /// Whether SOF reaches the MCU (U2 and U3).
    pub const fn has_sof(self) -> bool {
        self.is_attached()
    }

    /// Whether the host drains the IN endpoint: only U3. Whether a host OS drains IN with no port
    /// open is UNVERIFIED; no drain is what the IDF SOF monitor and the VFS 50 ms stall assume.
    pub const fn drains_in(self) -> bool {
        matches!(self, UsbHostState::U3)
    }

    pub const fn name(self) -> &'static str {
        match self {
            UsbHostState::U0 => "U0 DETACHED",
            UsbHostState::U1 => "U1 CHARGE_ONLY",
            UsbHostState::U2 => "U2 ATTACHED_IDLE",
            UsbHostState::U3 => "U3 ATTACHED_OPEN",
        }
    }
}

/// What a `SET_CONTROL_LINE_STATE` did to the chip: the rows of TRM Table 30.3-2.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LineAction {
    /// (RTS 0, DTR 0): the download flag is cleared.
    ClearDownload,
    /// (RTS 0, DTR 1): the download flag is set.
    SetDownload,
    /// (RTS 1, DTR 0): a system reset with cause `USB_UART_CHIP` (0x15), with `GPIO_STRAP` latched
    /// from the download flag.
    Reset {
        /// The strap to latch: the download strap when the flag was set, else the board strap.
        strap: u8,
    },
    /// (RTS 1, DTR 1): nothing happens.
    None,
}

/// The board's normal strap value, `boot:0xa`.
pub const STRAP_FLASH_BOOT: u8 = 0x0A;
/// The strap latched when the download flag is set: `boot:0x6 (DOWNLOAD(USB/UART0))`. Class A:
/// the device latches it on esptool's USB-JTAG-serial download reset (`specs/blocks/usj.toml`).
/// 0x02 (`UART0_BOOT`) would never answer esptool over USB (`specs/notes/usj-download-strap.md`).
pub const STRAP_DOWNLOAD: u8 = 0x06;

/// The `[usb]` timing. The flash-boot strap has one row (`[soc] strap`) and reaches
/// [`UsbPlug::set_line`] as an argument.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UsbConfig {
    /// Enumeration delay in milliseconds: 0 under the `fast` timing profile, 100 under `device`
    /// (UNVERIFIED, measured against a macOS USB host).
    pub enumerate_ms: u64,
    /// The strap value latched with the download flag set ([`STRAP_DOWNLOAD`]).
    pub download_strap: u8,
}

impl Default for UsbConfig {
    fn default() -> Self {
        UsbConfig {
            enumerate_ms: 0,
            download_strap: STRAP_DOWNLOAD,
        }
    }
}

/// The USB plug model. Holds the cable, client and PHY facts plus the line state, and derives
/// [`UsbPlug::state`] with the MCU rail, so nothing is U3 with the cable out.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct UsbPlug {
    cfg: UsbConfig,
    cable: bool,
    client_open: bool,
    /// False while the USJ PHY is powered down (deep sleep): the host sees a detach.
    phy_on: bool,
    dtr: bool,
    rts: bool,
    download_flag: bool,
}

impl Default for UsbPlug {
    /// The cable plugged and a client holding the port open. Without a client the IDF VFS stalls 50
    /// ms per blocked burst and drops bytes, so goldens would differ from a monitored device.
    fn default() -> Self {
        UsbPlug {
            cfg: UsbConfig::default(),
            cable: true,
            client_open: true,
            phy_on: true,
            dtr: false,
            rts: false,
            download_flag: false,
        }
    }
}

impl UsbPlug {
    pub fn new(cfg: UsbConfig) -> Self {
        UsbPlug {
            cfg,
            ..UsbPlug::default()
        }
    }

    pub fn config(&self) -> &UsbConfig {
        &self.cfg
    }

    /// The U-state: enumerated needs cable, rail and PHY; U3 also needs an open port.
    pub fn state(&self, rail: RailState) -> UsbHostState {
        if !self.cable {
            return UsbHostState::U0;
        }
        if !rail.is_on() || !self.phy_on {
            return UsbHostState::U1;
        }
        if self.client_open {
            UsbHostState::U3
        } else {
            UsbHostState::U2
        }
    }

    /// `usb.cable(bool)`. Unplugging drops the client too: the endpoint vanishes on the host.
    pub fn set_cable(&mut self, plugged: bool) {
        self.cable = plugged;
        if !plugged {
            self.client_open = false;
        }
    }

    /// A host client opened or closed the CDC-ACM port; with the cable out nothing opens.
    pub fn set_client(&mut self, open: bool) {
        self.client_open = open && self.cable;
    }

    pub fn cable(&self) -> bool {
        self.cable
    }

    pub fn client_open(&self) -> bool {
        self.client_open
    }

    /// The USJ PHY power state: false in deep sleep, when the host sees a detach.
    pub fn set_phy(&mut self, on: bool) {
        self.phy_on = on;
    }

    pub fn phy_on(&self) -> bool {
        self.phy_on
    }

    /// The current line state, `(dtr, rts)`.
    pub fn line(&self) -> (bool, bool) {
        (self.dtr, self.rts)
    }

    pub fn download_flag(&self) -> bool {
        self.download_flag
    }

    /// Evaluates TRM Table 30.3-2 on every line state; `strap` is the flash-boot strap. A repeated
    /// identical state is not a second reset (esptool writes duplicates for `usbser.sys`). As on
    /// silicon, `ClassicReset` flash-boots; its download mode is emulated in
    /// `pemu_host::endpoints::rfc2217::ResetBridge`, never here.
    pub fn set_line(&mut self, dtr: bool, rts: bool, strap: u8) -> LineAction {
        let repeated = self.dtr == dtr && self.rts == rts;
        self.dtr = dtr;
        self.rts = rts;
        match (rts, dtr) {
            (false, false) => {
                self.download_flag = false;
                LineAction::ClearDownload
            }
            (false, true) => {
                self.download_flag = true;
                LineAction::SetDownload
            }
            (true, false) if !repeated => LineAction::Reset {
                strap: if self.download_flag {
                    self.cfg.download_strap
                } else {
                    strap
                },
            },
            // A repeated (1,0) is esptool's duplicate write, not a second reset.
            (true, false) => LineAction::None,
            (true, true) => LineAction::None,
        }
    }

    /// Resets the line state on re-enumeration (UNVERIFIED); the download flag is chip state and
    /// stays.
    pub fn reset_line(&mut self) {
        self.dtr = false;
        self.rts = false;
    }
}
