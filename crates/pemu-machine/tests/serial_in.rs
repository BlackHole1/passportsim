//! Journaled host bytes reach the guest: the real ROM, reset into USB download mode, answers an
//! esptool `SYNC` journaled as `InputEvent::SerialIn`, and a replay reaches the same `state_hash`.

#![cfg(feature = "bundled-rom")]

use pemu_core::hostio::SerialStream;
use pemu_core::input::{InputEvent, SerialChan};
use pemu_core::time::VTime;
use pemu_loader::bundle::FlashImage;
use pemu_loader::efuse_image::EfuseImage;
use pemu_machine::config::{Assets, MachineConfig};
use pemu_machine::machine::{At, Machine};
use pemu_machine::run::RunLimits;
use pemu_machine::stops::StopSet;

/// An esptool `SYNC` request in SLIP framing: header, `07 07 12 20`, 32 x `0x55`.
fn sync_request() -> Vec<u8> {
    let mut p = vec![
        0xC0, 0x00, 0x08, 0x24, 0x00, 0, 0, 0, 0, 0x07, 0x07, 0x12, 0x20,
    ];
    p.extend([0x55; 32]);
    p.push(0xC0);
    p
}

fn run_to(m: &mut Machine, ms: u64) {
    m.run(RunLimits {
        until: Some(VTime::from_ms(ms)),
        max_insns: None,
        stops: StopSet::default(),
    });
}

/// Download reset at 10 ms, `SYNC` at 300 ms, run to 400 ms. Returns the console and state hash.
fn session() -> (Vec<u8>, [u8; 32]) {
    let assets = Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(7))
        .expect("the bundled ROM is pinned");
    let mut m = Machine::new(MachineConfig::default(), assets).expect("machine");
    // (RTS 0, DTR 1) sets the download flag, (RTS 1, DTR 0) resets with it, (0, 0) clears it.
    for (dtr, rts) in [(true, false), (false, true), (false, false)] {
        m.input(At::Vt(VTime::from_ms(10)), InputEvent::UsbLine { dtr, rts })
            .expect("a future instant");
    }
    m.input(
        At::Vt(VTime::from_ms(300)),
        InputEvent::SerialIn {
            chan: SerialChan::USJ,
            data: sync_request(),
        },
    )
    .expect("a future instant");
    run_to(&mut m, 400);
    let console: Vec<u8> = m
        .io()
        .serial_ring(SerialStream::UsjTx)
        .slices(0)
        .iter()
        .copied()
        .collect();
    assert_eq!(m.unapplied_inputs(), 0, "every input reached a consumer");
    (console, m.state_hash())
}

#[test]
fn a_journaled_sync_reaches_the_rom_and_replays_to_the_same_state_hash() {
    let (console, hash) = session();
    let text = String::from_utf8_lossy(&console);
    assert!(
        text.contains("rst:0x15 (USB_UART_CHIP_RESET),boot:0x6 (DOWNLOAD(USB/UART0))"),
        "{text}"
    );
    // The ROM's SYNC response: direction 1, opcode 8, the four magic bytes echoed in `val`.
    let response = [0xC0, 0x01, 0x08, 0x04, 0x00, 0x07, 0x07, 0x12, 0x20];
    assert!(
        console.windows(response.len()).any(|w| w == response),
        "the ROM answered the SYNC it read from the OUT endpoint"
    );
    let (again, replay) = session();
    assert_eq!(again, console);
    assert_eq!(
        replay, hash,
        "a replay of the journal reaches the same state"
    );
}
