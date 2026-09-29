# USB Serial/JTAG download strap note

This note records the strap value a USB Serial/JTAG download reset latches, and corrects the value
an earlier draft of the USJ spec recommended. It restates behavior only: what the device and the
bundled mask ROM observably did.

- **Author role.** Written from a device capture and from runs of the bundled ROM in the emulator,
  both black-box observations.
- **How to cite.** `specs/notes/usj-download-strap.md` section id, for example
  `usj-download-strap usj-strap-device`.

## Index

| Id | Title | Used by |
|---|---|---|
| usj-strap-device | The device latches 0x06 | `specs/blocks/usj.toml` |
| usj-strap-draft-correction | An earlier draft recommended 0x02 | `specs/blocks/usj.toml`, `boards/ai-passport.toml`, `crates/pemu-board/src/usb_plug.rs`, `crates/pemu-soc-c3/src/periph/gpio.rs` |
| usj-strap-rom-branches | How the ROM reads the low strap bits | `crates/pemu-soc-c3/src/periph/gpio.rs` |

## usj-strap-device

- **Fact.** After esptool's USB-JTAG-serial reset sequence the device prints
  `rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))`; after a hard reset it prints
  `boot:0xa (SPI_FAST_FLASH_BOOT)`.
- **ROM.** ESP-ROM esp32c3-eco7-20230720.
- **Source.** Device capture of the USB Serial/JTAG download reset and of a hard reset.
- **Confidence.** Measured on the device. Class A.

## usj-strap-draft-correction

- **Superseded value.** An earlier draft of the USJ spec recommended 0x02, the board strap 0x0A
  with the GPIO9 bit cleared.
- **Why it is wrong.** The ROM contradicts it. With 0x02 latched, the bundled ECO7 ROM prints
  `boot:0x2 (UART0_BOOT)` and waits for a download on UART0 only, so an esptool `SYNC` sent over
  USB Serial/JTAG gets no reply. Measured by running the bundled ROM in the emulator.
- **Other candidate.** 0x00, which the same ROM prints as `boot:0x0 (USB_BOOT)`, also answers
  `SYNC` on USB Serial/JTAG, but the device capture above shows 0x06.

## usj-strap-rom-branches

- **Observed in the emulator** with the bundled ROM, each value latched by a cause 0x15 reset:
  - 0x02 prints `boot:0x2 (UART0_BOOT)` and waits on UART0 only;
  - 0x00 prints `boot:0x0 (USB_BOOT)` and answers `SYNC` on USB Serial/JTAG;
  - 0x06 and 0x07 print `DOWNLOAD(USB/UART0)` and answer `SYNC` on USB Serial/JTAG.
- **Decode.** The ROM takes its combined UART-or-USB download branch when the strap's bits 2 and 3
  read 0b01 (`strap & 0xC == 4`), which is why 0x06 serves the download protocol on USB
  Serial/JTAG as well as on UART0.
- **Confidence.** Measured on an oracle (the emulator running the unmodified mask ROM as a black
  box); the decode is a reading of those runs, UNVERIFIED here.
