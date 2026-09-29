//! The U0 to U3 table and the RFC 2217 line-state decoder.

use pemu_board::passport::{BoardConfig, PassportBoard};
use pemu_board::power::RailState;
use pemu_board::traits::{BoardCx, BoardPorts};
use pemu_board::usb_plug::{LineAction, STRAP_DOWNLOAD, STRAP_FLASH_BOOT, UsbHostState, UsbPlug};
use pemu_core::input::InputEvent;
use pemu_core::time::VTime;

/// Derived from cable, rail and client rather than stored, so no combination names a state the
/// table does not have.
#[test]
fn the_u_states_are_the_documented_table() {
    let mut plug = UsbPlug::default();

    // The default: cable plugged, rail up, a client holding the port open.
    assert_eq!(plug.state(RailState::On), UsbHostState::U3);
    assert_eq!(plug.state(RailState::Off), UsbHostState::U1);

    plug.set_client(false);
    assert_eq!(plug.state(RailState::On), UsbHostState::U2);

    plug.set_cable(false);
    assert_eq!(plug.state(RailState::On), UsbHostState::U0);
    assert_eq!(plug.state(RailState::Off), UsbHostState::U0);

    plug.set_cable(true);
    plug.set_client(true);
    assert_eq!(plug.state(RailState::On), UsbHostState::U3);
}

#[test]
fn only_u3_drains_the_in_endpoint_and_only_u2_and_u3_see_sof() {
    assert!(!UsbHostState::U0.has_sof() && !UsbHostState::U0.drains_in());
    assert!(!UsbHostState::U1.has_sof() && !UsbHostState::U1.drains_in());
    assert!(UsbHostState::U2.has_sof() && !UsbHostState::U2.drains_in());
    assert!(UsbHostState::U3.has_sof() && UsbHostState::U3.drains_in());
    assert!(!UsbHostState::U0.is_attached() && !UsbHostState::U1.is_attached());
    assert!(UsbHostState::U2.is_attached() && UsbHostState::U3.is_attached());
}

/// Plugging back in does not silently reopen the port.
#[test]
fn unplugging_drops_the_client() {
    let mut plug = UsbPlug::default();
    plug.set_cable(false);
    assert!(!plug.client_open());
    plug.set_client(true);
    assert!(!plug.client_open(), "no cable, nothing to open");
    plug.set_cable(true);
    assert_eq!(plug.state(RailState::On), UsbHostState::U2);
}

/// Deep sleep powers the USJ PHY off; on wake the state comes back.
#[test]
fn the_phy_powering_down_detaches_the_host() {
    let mut plug = UsbPlug::default();
    plug.set_phy(false);
    assert_eq!(plug.state(RailState::On), UsbHostState::U1);
    plug.set_phy(true);
    assert_eq!(plug.state(RailState::On), UsbHostState::U3);
}

/// TRM Table 30.3-2: all four rows, evaluated on every line state.
#[test]
fn the_line_state_table_is_evaluated_on_every_event() {
    let mut plug = UsbPlug::default();
    assert_eq!(
        plug.set_line(false, false, STRAP_FLASH_BOOT),
        LineAction::ClearDownload
    );
    assert!(!plug.download_flag());
    assert_eq!(
        plug.set_line(true, false, STRAP_FLASH_BOOT),
        LineAction::SetDownload
    );
    assert!(plug.download_flag());
    assert_eq!(
        plug.set_line(true, true, STRAP_FLASH_BOOT),
        LineAction::None
    );
    assert!(plug.download_flag(), "(1,1) changes nothing");
    assert_eq!(plug.line(), (true, true));
}

/// esptool's `USBJTAGSerialReset`: (0,0), (DTR 1), then (RTS 1, DTR 0) resets with the download
/// flag set.
#[test]
fn the_esptool_download_sequence_resets_with_the_download_strap() {
    let mut plug = UsbPlug::default();
    plug.set_line(false, false, STRAP_FLASH_BOOT);
    plug.set_line(true, false, STRAP_FLASH_BOOT);
    assert_eq!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::Reset {
            strap: STRAP_DOWNLOAD
        }
    );
}

/// esptool's hard reset after flashing: (0,0) clears the flag, so a later RTS reset boots the
/// app with the board strap `boot:0xa`.
#[test]
fn a_later_rts_reset_boots_the_app_rather_than_the_rom_downloader() {
    let mut plug = UsbPlug::default();
    plug.set_line(true, false, STRAP_FLASH_BOOT);
    plug.set_line(false, true, STRAP_FLASH_BOOT);
    plug.set_line(false, false, STRAP_FLASH_BOOT);
    assert!(!plug.download_flag());
    assert_eq!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::Reset {
            strap: STRAP_FLASH_BOOT
        }
    );
}

/// esptool's `ClassicReset` (DTR low, RTS high, DTR high, RTS low, DTR low, each change its own
/// control request) reaches the reset row (RTS 1, DTR 0) before the flag-setting row, so the chip
/// flash-boots. The download mode a `ClassicReset` client gets over `rfc2217://` comes from
/// `pemu_host::endpoints::rfc2217::ResetBridge`, not from the chip.
#[test]
fn the_esptool_classic_reset_resets_before_it_sets_the_download_flag() {
    let mut plug = UsbPlug::default();
    assert_eq!(
        plug.set_line(false, false, STRAP_FLASH_BOOT),
        LineAction::ClearDownload
    );
    // (DTR 0, RTS 1): EN low. This is the reset row, and the flag is still clear.
    assert_eq!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::Reset {
            strap: STRAP_FLASH_BOOT
        },
        "the classic reset fires with the flag clear, so the ROM boots the app"
    );
    // (DTR 1, RTS 1): IO0 low. The table says nothing happens.
    assert_eq!(
        plug.set_line(true, true, STRAP_FLASH_BOOT),
        LineAction::None
    );
    // (DTR 1, RTS 0): EN high. Only now is the download flag set, after the reset.
    assert_eq!(
        plug.set_line(true, false, STRAP_FLASH_BOOT),
        LineAction::SetDownload
    );
    // (DTR 0, RTS 0): IO0 high again, and the flag is cleared with it.
    assert_eq!(
        plug.set_line(false, false, STRAP_FLASH_BOOT),
        LineAction::ClearDownload
    );
    assert!(
        !plug.download_flag(),
        "the sequence ends with no download flag set at all"
    );
}

/// Tolerates the duplicate DTR writes esptool injects for `usbser.sys`.
#[test]
fn a_repeated_line_state_is_not_a_second_reset() {
    let mut plug = UsbPlug::default();
    plug.set_line(false, false, STRAP_FLASH_BOOT);
    assert!(matches!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::Reset { .. }
    ));
    assert_eq!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::None
    );
    assert_eq!(
        plug.set_line(false, true, STRAP_FLASH_BOOT),
        LineAction::None
    );
}

/// Resetting the line to 0 is the UNVERIFIED default; the download flag is chip state.
#[test]
fn re_enumeration_resets_the_line_but_keeps_the_download_flag() {
    let mut plug = UsbPlug::default();
    plug.set_line(true, false, STRAP_FLASH_BOOT);
    assert!(plug.download_flag());
    plug.reset_line();
    assert_eq!(plug.line(), (false, false));
    assert!(plug.download_flag());
}

#[test]
fn the_board_reports_every_u_state_change() {
    let mut board = PassportBoard::from_toml(&BoardConfig::default());
    let mut cx = BoardCx::new(VTime(0));
    assert_eq!(board.usb(), UsbHostState::U3);

    let closed = board.apply_input(
        VTime::from_ms(1),
        &InputEvent::UsbClient { open: false },
        &mut cx,
    );
    assert_eq!(closed.usb, Some(UsbHostState::U2));

    let unplugged = board.apply_input(
        VTime::from_ms(2),
        &InputEvent::UsbCable { plugged: false },
        &mut cx,
    );
    assert_eq!(unplugged.usb, Some(UsbHostState::U0));

    let again = board.apply_input(
        VTime::from_ms(3),
        &InputEvent::UsbCable { plugged: false },
        &mut cx,
    );
    assert_eq!(again.usb, None);

    let reset = board.apply_input(
        VTime::from_ms(4),
        &InputEvent::UsbLine {
            dtr: false,
            rts: true,
        },
        &mut cx,
    );
    let line = reset.reset.expect("(RTS 1, DTR 0) resets the chip");
    assert_eq!(line.cause, pemu_core::reset::ResetCause::USB_UART_CHIP);
    assert_eq!(line.strap, STRAP_FLASH_BOOT);
}
