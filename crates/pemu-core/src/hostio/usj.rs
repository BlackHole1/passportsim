//! The USB-Serial-JTAG host state of `HostIo::usj_ctrl`: cable, client, line state and
//! enumeration.

use serde::{Deserialize, Serialize};

/// How far USB enumeration has got. UNVERIFIED variant set: only the delay is known.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum UsjEnumeration {
    /// Not enumerated: no cable, or the MCU rail is off.
    #[default]
    Detached,
    /// Cable plugged and the rail on; the enumeration delay is still running, so no SOF yet.
    Pending,
    /// CDC-ACM enumerated: SOF runs and `FRAME_NUM` advances.
    Enumerated,
}

/// Host state of the USB cable and client.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum UsbHostState {
    /// U0 DETACHED: unplugged, no SOF, `FRAME_NUM` frozen, the IN FIFO never drains.
    #[default]
    Detached,
    /// U1 CHARGE_ONLY: plugged with the MCU rail off.
    ChargeOnly,
    /// U2 ATTACHED_IDLE: enumerated with no client; SOF runs.
    AttachedIdle,
    /// U3 ATTACHED_OPEN: a client has the port open; the IN FIFO drains per profile.
    AttachedOpen,
}

/// USB-Serial-JTAG control state of `HostIo::usj_ctrl`: cable, client open, RFC 2217 DTR and RTS,
/// and enumeration. Host writes are journaled as `InputEvent::UsbCable`, `UsbClient` and
/// `UsbLine` before any model sees them. The setters are plain assignments; the transitions
/// (enumeration delay, line-state table, deep-sleep detach) belong to the USJ model.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsjCtrl {
    cable: bool,
    client_open: bool,
    dtr: bool,
    rts: bool,
    enumeration: UsjEnumeration,
}

impl Default for UsjCtrl {
    /// Cable plugged, a client with the port open, enumerated (so U3 once the rail is on), line
    /// state 0.
    fn default() -> Self {
        UsjCtrl {
            cable: true,
            client_open: true,
            dtr: false,
            rts: false,
            enumeration: UsjEnumeration::Enumerated,
        }
    }
}

impl UsjCtrl {
    pub fn cable(&self) -> bool {
        self.cable
    }

    pub fn set_cable(&mut self, plugged: bool) {
        self.cable = plugged;
    }

    pub fn client_open(&self) -> bool {
        self.client_open
    }

    pub fn set_client_open(&mut self, open: bool) {
        self.client_open = open;
    }

    pub fn dtr(&self) -> bool {
        self.dtr
    }

    pub fn rts(&self) -> bool {
        self.rts
    }

    /// Sets both line-state bits (`InputEvent::UsbLine`). The line-state table is evaluated on
    /// every write, so the caller applies its row even when neither bit changed.
    pub fn set_line_state(&mut self, rts: bool, dtr: bool) {
        self.rts = rts;
        self.dtr = dtr;
    }

    pub fn enumeration(&self) -> UsjEnumeration {
        self.enumeration
    }

    pub fn set_enumeration(&mut self, state: UsjEnumeration) {
        self.enumeration = state;
    }

    /// The U-state for this control state; the rail is board state, so the caller passes it.
    pub fn host_state(&self, rail_on: bool) -> UsbHostState {
        match (self.cable, rail_on, self.client_open) {
            (false, _, _) => UsbHostState::Detached,
            (true, false, _) => UsbHostState::ChargeOnly,
            (true, true, false) => UsbHostState::AttachedIdle,
            (true, true, true) => UsbHostState::AttachedOpen,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usj_ctrl_maps_cable_client_and_rail_to_the_u_states() {
        let mut ctrl = UsjCtrl::default();
        assert_eq!(ctrl.host_state(true), UsbHostState::AttachedOpen); // U3
        ctrl.set_client_open(false);
        assert_eq!(ctrl.host_state(true), UsbHostState::AttachedIdle); // U2
        assert_eq!(ctrl.host_state(false), UsbHostState::ChargeOnly); // U1
        ctrl.set_cable(false);
        ctrl.set_enumeration(UsjEnumeration::Detached);
        assert_eq!(ctrl.host_state(true), UsbHostState::Detached); // U0

        // Line state keeps both bits; the table is the caller's to apply.
        assert_eq!((ctrl.rts(), ctrl.dtr()), (false, false));
        ctrl.set_line_state(true, false);
        assert_eq!((ctrl.rts(), ctrl.dtr()), (true, false));
        ctrl.set_line_state(false, true);
        assert_eq!((ctrl.rts(), ctrl.dtr()), (false, true));
        assert_eq!(ctrl.enumeration(), UsjEnumeration::Detached);
    }
}
