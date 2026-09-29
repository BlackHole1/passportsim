//! Protocol auto-detect of the TCP endpoint on the first byte: a pyserial RFC 2217 client sends
//! Telnet `IAC` at once, esptool over `socket://` starts with a SLIP delimiter, and a monitor sends
//! nothing, so silence means a raw console.

use std::time::Duration;

use super::rfc2217::IAC;

/// How long the endpoint waits for a first byte before treating the client as a raw console.
pub const SILENCE: Duration = Duration::from_millis(300);

/// The SLIP frame delimiter esptool's first packet starts with.
pub const SLIP_END: u8 = 0xC0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Telnet `IAC` first: RFC 2217, `SET-CONTROL` DTR and RTS become `UsbLine` inputs.
    Rfc2217,
    Slip,
    /// Silence, or any other first byte: bidirectional USJ bytes.
    Console,
}

impl Mode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Mode::Rfc2217 => "rfc2217",
            Mode::Slip => "slip",
            Mode::Console => "console",
        }
    }
}

/// Classifies a connection by its first byte, `None` meaning [`SILENCE`] passed. Any byte other
/// than `IAC` or the SLIP delimiter is console data for the guest.
pub fn classify(first: Option<u8>) -> Mode {
    match first {
        Some(IAC) => Mode::Rfc2217,
        Some(SLIP_END) => Mode::Slip,
        _ => Mode::Console,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_byte_selects_the_mode() {
        assert_eq!(classify(Some(0xFF)), Mode::Rfc2217);
        assert_eq!(classify(Some(0xC0)), Mode::Slip);
        assert_eq!(classify(None), Mode::Console);
        assert_eq!(classify(Some(b'h')), Mode::Console);
        assert_eq!(SILENCE, Duration::from_millis(300));
    }
}
