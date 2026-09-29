//! The ST7789P3 panel and the backlight.
//!
//! Streams are framed as `esp_lcd_panel_io_spi` frames them: the command byte in a DC-low
//! transaction that keeps CS asserted when parameters follow, then parameters or pixel chunks in
//! DC-high transactions, the last of which releases CS.

use pemu_board::backlight::{Backlight, Brightness};
use pemu_board::st7789::{
    BOOT_SEQUENCE, BootStep, FrameView, MAX_PARAMS, PANEL_HEIGHT, PANEL_PIXELS, PANEL_WIDTH,
    PanelConfig, St7789p3, TimingLint, cmd,
};
use pemu_board::traits::{BoardDomain, Chip};
use pemu_core::hostio::FramePort;
use pemu_core::time::VTime;

/// A powered panel out of sleep with the display on, where the boot sequence leaves it.
fn lit_panel() -> St7789p3 {
    let mut panel = St7789p3::default();
    panel.set_powered(true);
    tx_param(&mut panel, at(200), cmd::SLPOUT, &[]);
    tx_param(&mut panel, at(300), cmd::COLMOD, &[0x55]);
    tx_param(&mut panel, at(301), cmd::INVON, &[]);
    tx_param(&mut panel, at(302), cmd::DISPON, &[]);
    panel
}

fn at(ms: u64) -> VTime {
    VTime::from_ms(ms)
}

/// One `panel_io_spi_tx_param` transaction pair.
fn tx_param(panel: &mut St7789p3, t: VTime, command: u8, params: &[u8]) {
    panel.transfer(t, false, &[command], params.is_empty());
    if !params.is_empty() {
        panel.transfer(t, true, params, true);
    }
}

/// One `panel_io_spi_tx_color` transaction group: the command and every chunk but the last keep
/// CS asserted.
fn tx_color(panel: &mut St7789p3, t: VTime, command: u8, chunks: &[&[u8]]) {
    panel.transfer(t, false, &[command], false);
    for (index, chunk) in chunks.iter().enumerate() {
        panel.transfer(t, true, chunk, index + 1 == chunks.len());
    }
}

/// The CASET or RASET parameters for an inclusive range.
fn window_params(start: u16, end: u16) -> [u8; 4] {
    [(start >> 8) as u8, start as u8, (end >> 8) as u8, end as u8]
}

/// `n` pixels of the same colour, big-endian on the wire.
fn pixels(colour: u16, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n * 2);
    for _ in 0..n {
        out.push((colour >> 8) as u8);
        out.push(colour as u8);
    }
    out
}

fn set_window(panel: &mut St7789p3, t: VTime, xs: u16, xe: u16, ys: u16, ye: u16) {
    tx_param(panel, t, cmd::CASET, &window_params(xs, xe));
    tx_param(panel, t, cmd::RASET, &window_params(ys, ye));
}

fn raw_at(panel: &St7789p3, x: u16, y: u16) -> u16 {
    panel.raw()[y as usize * PANEL_WIDTH as usize + x as usize]
}

#[test]
fn caset_and_raset_set_the_inclusive_window() {
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 10, 20, 30, 40);
    assert_eq!(panel.window(), (10, 20, 30, 40));
}

#[test]
fn caset_and_raset_clip_past_column_239_and_row_319() {
    // CASET defaults to 0..0xEF and RASET to 0..0x13F, so an address past the end has nowhere to
    // land.
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 0x01FF, 0, 0x03FF);
    assert_eq!(panel.window(), (0, PANEL_WIDTH - 1, 0, PANEL_HEIGHT - 1));

    set_window(&mut panel, at(401), 400, 500, 400, 500);
    assert_eq!(
        panel.window(),
        (
            PANEL_WIDTH - 1,
            PANEL_WIDTH - 1,
            PANEL_HEIGHT - 1,
            PANEL_HEIGHT - 1
        )
    );

    tx_color(&mut panel, at(402), cmd::RAMWR, &[&pixels(0xBEEF, 1)]);
    assert_eq!(raw_at(&panel, PANEL_WIDTH - 1, PANEL_HEIGHT - 1), 0xBEEF);
}

#[test]
fn ramwr_fills_the_window_left_to_right_and_top_to_bottom() {
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 4, 6, 8, 9);
    let mut stream = Vec::new();
    for value in 1u16..=6 {
        stream.extend_from_slice(&value.to_be_bytes());
    }
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&stream]);

    assert_eq!(raw_at(&panel, 4, 8), 1);
    assert_eq!(raw_at(&panel, 5, 8), 2);
    assert_eq!(raw_at(&panel, 6, 8), 3);
    assert_eq!(raw_at(&panel, 4, 9), 4);
    assert_eq!(raw_at(&panel, 5, 9), 5);
    assert_eq!(raw_at(&panel, 6, 9), 6);
    assert_eq!(raw_at(&panel, 7, 8), 0);
    assert_eq!(raw_at(&panel, 4, 10), 0);
    assert_eq!(panel.pixels_written(), 6);
    assert_eq!(panel.ramwr_count(), 1);
}

#[test]
fn ramwr_resets_the_pointer_and_ramwrc_continues_it() {
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 1, 0, 1);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(0x1111, 2)]);
    assert_eq!(panel.cursor(), (0, 1));

    tx_color(&mut panel, at(402), cmd::RAMWRC, &[&pixels(0x2222, 2)]);
    assert_eq!(raw_at(&panel, 0, 0), 0x1111);
    assert_eq!(raw_at(&panel, 1, 0), 0x1111);
    assert_eq!(raw_at(&panel, 0, 1), 0x2222);
    assert_eq!(raw_at(&panel, 1, 1), 0x2222);

    tx_color(&mut panel, at(403), cmd::RAMWR, &[&pixels(0x3333, 1)]);
    assert_eq!(raw_at(&panel, 0, 0), 0x3333);
    assert_eq!(raw_at(&panel, 1, 0), 0x1111);
}

#[test]
fn an_odd_pixel_byte_carries_into_the_next_segment() {
    // The driver splits a flush into chunks under one CS assertion, so the halves of a pixel can
    // arrive in different transactions.
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 1, 0, 0);
    panel.transfer(at(401), false, &[cmd::RAMWR], false);
    panel.transfer(at(401), true, &[0xAB, 0xCD, 0x12], false);
    assert_eq!(panel.carry(), Some(0x12));
    assert_eq!(raw_at(&panel, 0, 0), 0xABCD);
    assert_eq!(raw_at(&panel, 1, 0), 0);

    panel.transfer(at(402), true, &[0x34], true);
    assert_eq!(panel.carry(), None);
    assert_eq!(raw_at(&panel, 1, 0), 0x1234);
}

#[test]
fn the_pixel_pointer_wraps_inside_the_window() {
    // x wraps to xs with y + 1, and y wraps to ys after ye.
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 2, 3, 5, 6);
    let mut stream = Vec::new();
    for value in 1u16..=5 {
        stream.extend_from_slice(&value.to_be_bytes());
    }
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&stream]);

    assert_eq!(raw_at(&panel, 2, 5), 5);
    assert_eq!(raw_at(&panel, 3, 5), 2);
    assert_eq!(raw_at(&panel, 2, 6), 3);
    assert_eq!(raw_at(&panel, 3, 6), 4);
    assert_eq!(panel.cursor(), (3, 5));
}

#[test]
fn madctl_mv_swaps_the_axes() {
    let mut panel = lit_panel();
    tx_param(&mut panel, at(400), cmd::MADCTL, &[0x20]);
    assert_eq!(panel.madctl(), 0x20);
    // With MV the column range addresses rows, so CASET may reach 319 and RASET clips at 239.
    set_window(&mut panel, at(401), 0, 0x03FF, 0, 0x01FF);
    assert_eq!(panel.window(), (0, PANEL_HEIGHT - 1, 0, PANEL_WIDTH - 1));

    set_window(&mut panel, at(402), 7, 8, 3, 3);
    tx_color(&mut panel, at(403), cmd::RAMWR, &[&pixels(0x0F0F, 2)]);
    assert_eq!(raw_at(&panel, 3, 7), 0x0F0F);
    assert_eq!(raw_at(&panel, 3, 8), 0x0F0F);
    assert_eq!(raw_at(&panel, 7, 3), 0);
}

#[test]
fn madctl_mx_and_my_mirror_the_axes() {
    let mut panel = lit_panel();
    tx_param(&mut panel, at(400), cmd::MADCTL, &[0x40]);
    set_window(&mut panel, at(401), 0, 0, 0, 0);
    tx_color(&mut panel, at(402), cmd::RAMWR, &[&pixels(0x00FF, 1)]);
    assert_eq!(raw_at(&panel, PANEL_WIDTH - 1, 0), 0x00FF);

    tx_param(&mut panel, at(403), cmd::MADCTL, &[0x80]);
    tx_color(&mut panel, at(404), cmd::RAMWR, &[&pixels(0xFF00, 1)]);
    assert_eq!(raw_at(&panel, 0, PANEL_HEIGHT - 1), 0xFF00);

    tx_param(&mut panel, at(405), cmd::MADCTL, &[0xC0]);
    tx_color(&mut panel, at(406), cmd::RAMWR, &[&pixels(0x0FF0, 1)]);
    assert_eq!(raw_at(&panel, PANEL_WIDTH - 1, PANEL_HEIGHT - 1), 0x0FF0);
}

#[test]
fn invon_shows_ram_is_a_switch() {
    // With INVON the glass shows panel memory unmodified (g3-menu-colours-invon); INVOFF shows the
    // complement.
    const UI_SKY: u16 = 0x145D;
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 0, 0, 0);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(UI_SKY, 1)]);

    assert!(panel.inverted());
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), UI_SKY);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), UI_SKY);

    tx_param(&mut panel, at(402), cmd::INVOFF, &[]);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), UI_SKY);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), !UI_SKY);

    panel.set_invon_shows_ram(false);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), UI_SKY);
    tx_param(&mut panel, at(403), cmd::INVON, &[]);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), !UI_SKY);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), UI_SKY);
}

#[test]
fn glass_is_black_unless_powered_out_of_sleep_and_display_on() {
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 0, 0, 0);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(0x1234, 1)]);
    assert!(panel.lit());
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0x1234);

    tx_param(&mut panel, at(402), cmd::DISPOFF, &[]);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), 0x1234);

    tx_param(&mut panel, at(403), cmd::DISPON, &[]);
    tx_param(&mut panel, at(600), cmd::SLPIN, &[]);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), 0x1234);

    tx_param(&mut panel, at(800), cmd::SLPOUT, &[]);
    panel.set_powered(false);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), 0x1234);
}

#[test]
fn perceived_scales_glass_by_the_integer_backlight_pair() {
    let mut panel = lit_panel();
    set_window(&mut panel, at(400), 0, 0, 0, 0);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(0xFFFF, 1)]);

    let mut backlight = Backlight::default();
    assert_eq!(panel.pixel(FrameView::Perceived, 0, 0), 0);

    // `bsp_display_backlight(100)` writes duty 1023; the LEDC model hands the board the integer
    // duty.
    backlight.ledc(at(402), 0, 1023, 10, 5000);
    panel.set_backlight(&backlight);
    assert_eq!(panel.brightness(), Brightness::new(1023 << 4, 10));
    assert_eq!(panel.brightness().level(), 1023);
    assert_eq!(panel.brightness().scale(), 1024);
    // 31 * 1023 / 1024 = 30, 63 * 1023 / 1024 = 62: full white dims by one step of each channel.
    assert_eq!(panel.pixel(FrameView::Perceived, 0, 0), 0xF7DE);

    backlight.ledc(at(403), 0, 1023 / 2, 10, 5000);
    panel.set_backlight(&backlight);
    assert_eq!(panel.brightness().percent(), 49);
    assert_eq!(panel.pixel(FrameView::Perceived, 0, 0), 0x7BEF);

    backlight.ledc(at(404), 0, 0, 10, 5000);
    panel.set_backlight(&backlight);
    assert_eq!(panel.pixel(FrameView::Perceived, 0, 0), 0);
    assert_eq!(panel.pixel(FrameView::Raw, 0, 0), 0xFFFF);
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0xFFFF);
}

#[test]
fn backlight_brightness_is_an_integer_pair_with_polarity_and_output_enable() {
    let mut backlight = Backlight::default();
    assert_eq!(backlight.brightness(), Brightness::OFF);
    assert!(!backlight.enabled());

    backlight.ledc(at(1), 0, 1023, 10, 5000);
    assert_eq!(backlight.duty(), 1023);
    assert_eq!(backlight.duty_res(), 10);
    assert_eq!(backlight.freq_hz(), 5000);
    assert_eq!(backlight.brightness().percent(), 99);

    // With `sig_out_en` clear the pin rests at `idle_lv`.
    backlight.set_enabled(false);
    assert!(backlight.brightness().is_off());
    backlight.set_idle_level(true);
    assert_eq!(backlight.brightness().percent(), 100);
    backlight.set_enabled(true);

    backlight.ledc(at(2), 1, 0, 10, 5000);
    assert_eq!(backlight.duty(), 1023);

    let mut low = Backlight::new(false);
    low.ledc(at(3), 0, 1023, 10, 5000);
    assert_eq!(low.brightness().level(), 1);
    assert_eq!(low.brightness().scale(), 1024);

    backlight.reset(BoardDomain::Battery);
    assert_eq!(backlight.duty(), 1023);
    backlight.reset(BoardDomain::McuRail);
    assert_eq!(backlight.brightness(), Brightness::OFF);
    assert_eq!(backlight.updates(), 0);
}

#[test]
fn swreset_resets_the_registers_and_keeps_memory_madctl_and_colmod() {
    // SWRESET: sleep in, display off, INVOFF, full window; MADCTL, COLMOD and frame memory kept.
    let mut panel = lit_panel();
    tx_param(&mut panel, at(400), cmd::MADCTL, &[0x00]);
    set_window(&mut panel, at(401), 4, 6, 8, 9);
    tx_color(&mut panel, at(402), cmd::RAMWR, &[&pixels(0x0BAD, 1)]);

    tx_param(&mut panel, at(500), cmd::SWRESET, &[]);
    assert!(panel.sleeping());
    assert!(!panel.display_on());
    assert!(!panel.inverted());
    assert_eq!(panel.window(), (0, PANEL_WIDTH - 1, 0, PANEL_HEIGHT - 1));
    assert_eq!(panel.colmod(), 0x55);
    assert_eq!(panel.madctl(), 0x00);
    assert_eq!(raw_at(&panel, 4, 8), 0x0BAD);

    panel.reset(BoardDomain::Card);
    assert_eq!(raw_at(&panel, 4, 8), 0x0BAD);
    panel.reset(BoardDomain::BoardRail);
    assert_eq!(raw_at(&panel, 4, 8), 0);
    assert_eq!(panel.commands(), 0);
}

#[test]
fn vendor_commands_are_stored_and_the_last_value_wins() {
    // The boot table sends 0xD0 twice.
    let mut panel = lit_panel();
    tx_param(&mut panel, at(400), 0xD0, &[0xA7, 0xA1]);
    assert_eq!(panel.stored_params(0xD0), Some(&[0xA7, 0xA1][..]));
    tx_param(&mut panel, at(401), 0xD0, &[0xA4, 0xA1]);
    assert_eq!(panel.stored_params(0xD0), Some(&[0xA4, 0xA1][..]));
    assert_eq!(panel.stored_params(0xB2), None);

    let before = panel.raw().to_vec();
    tx_param(&mut panel, at(402), 0xFF, &[0x01, 0x02]);
    assert_eq!(panel.stored_params(0xFF), None);
    assert_eq!(panel.raw(), before.as_slice());
}

#[test]
fn the_timing_lint_warns_and_never_blocks() {
    let mut panel = St7789p3::default();
    panel.set_powered(true);

    tx_color(&mut panel, at(10), cmd::RAMWR, &[&pixels(0x1234, 1)]);
    assert_eq!(panel.warnings()[0].lint, TimingLint::RamwrWrongColmod);
    assert_eq!(raw_at(&panel, 0, 0), 0x1234, "the write still happened");

    let mut panel = St7789p3::default();
    panel.set_powered(true);
    tx_param(&mut panel, at(1000), cmd::SLPOUT, &[]);
    assert!(panel.warnings().is_empty());

    tx_param(&mut panel, at(1002), cmd::MADCTL, &[0x00]);
    assert_eq!(panel.warnings()[0].lint, TimingLint::CommandAfterSlpout);
    assert_eq!(panel.warnings()[0].cmd, cmd::MADCTL);
    assert_eq!(panel.madctl(), 0x00, "the command still applied");

    let warnings = panel.take_warnings();
    assert_eq!(warnings.len(), 1);
    assert!(
        panel.warnings().is_empty(),
        "taking the warnings clears them"
    );
    tx_param(&mut panel, at(1050), cmd::SLPIN, &[]);
    assert_eq!(panel.warnings()[0].lint, TimingLint::SleepCycleTooFast);
    assert!(panel.sleeping(), "the command still applied");

    // The panel powers up in sleep-in and the driver waits only 20 ms between SWRESET and SLPOUT,
    // breaking the 120 ms rule on every real boot, so SWRESET must count as a sleep-in edge. The
    // command still applies.
    let mut panel = St7789p3::default();
    tx_param(&mut panel, at(20), cmd::SWRESET, &[]);
    tx_param(&mut panel, at(40), cmd::SLPOUT, &[]);
    tx_param(&mut panel, at(140), cmd::MADCTL, &[0x00]);
    assert_eq!(panel.warnings().len(), 1, "{:?}", panel.warnings());
    assert_eq!(panel.warnings()[0].lint, TimingLint::SleepCycleTooFast);
    assert_eq!(panel.warnings()[0].cmd, cmd::SLPOUT);
    assert!(!panel.sleeping(), "the command still applied");
    assert_eq!(panel.madctl(), 0x00);

    let mut panel = St7789p3::default();
    tx_param(&mut panel, at(20), cmd::SWRESET, &[]);
    tx_param(&mut panel, at(200), cmd::SLPOUT, &[]);
    assert!(panel.warnings().is_empty(), "{:?}", panel.warnings());
}

#[test]
fn an_explicit_18_bpp_colmod_warns_instead_of_being_decoded_silently() {
    // 18 bpp is unmodeled (`colmod_18bpp` is Fidelity::U), so the guess must be announced, for both
    // the reset value and an explicit 0x66.
    let mut panel = St7789p3::default();
    panel.set_powered(true);
    tx_param(&mut panel, at(400), cmd::COLMOD, &[0x66]);
    assert_eq!(panel.colmod(), 0x66);
    tx_color(
        &mut panel,
        at(401),
        cmd::RAMWR,
        &[&[0x11, 0x22, 0x33, 0x44]],
    );
    assert_eq!(panel.warnings().len(), 1, "{:?}", panel.warnings());
    assert_eq!(panel.warnings()[0].lint, TimingLint::RamwrWrongColmod);
    assert_eq!(panel.warnings()[0].cmd, cmd::RAMWR);
    // The write still happens, decoded as RGB565: the lint never blocks.
    assert_eq!(raw_at(&panel, 0, 0), 0x1122);

    let warnings = panel.take_warnings();
    assert_eq!(warnings.len(), 1);
    tx_param(&mut panel, at(402), cmd::COLMOD, &[0x55]);
    tx_color(&mut panel, at(403), cmd::RAMWR, &[&[0x11, 0x22]]);
    assert!(panel.warnings().is_empty(), "{:?}", panel.warnings());
}

#[test]
fn a_parameter_list_decodes_the_same_however_the_transport_splits_it() {
    // `dc = 1` framing has no meaning, so CASET's four parameters decode the same in one segment or
    // two: the count applies the command, not an equality with the segment length.
    let split = {
        let mut panel = lit_panel();
        panel.transfer(at(400), false, &[cmd::CASET], false);
        panel.transfer(at(400), true, &[0x00, 0x05], false);
        panel.transfer(at(400), true, &[0x00, 0x40], true);
        let (xs, xe, _, _) = panel.window();
        (xs, xe)
    };
    assert_eq!(split, (5, 0x40));

    // Four parameters plus four bytes of overrun: the first four apply, once.
    let mut panel = lit_panel();
    panel.transfer(at(401), false, &[cmd::CASET], false);
    panel.transfer(
        at(401),
        true,
        &[0x00, 0x05, 0x00, 0x40, 0x00, 0x09, 0x00, 0x09],
        true,
    );
    assert_eq!((panel.window().0, panel.window().1), split);

    let mut panel = lit_panel();
    panel.transfer(at(402), false, &[cmd::CASET], false);
    panel.transfer(at(402), true, &[0x00, 0x05, 0x00], false);
    panel.transfer(at(402), true, &[0x40, 0x00, 0x09, 0x00, 0x09], true);
    assert_eq!((panel.window().0, panel.window().1), split);

    let mut panel = lit_panel();
    tx_param(&mut panel, at(403), cmd::MADCTL, &[0x20, 0x40]);
    assert_eq!(panel.madctl(), 0x20, "the first byte wins, not the last");
    tx_param(&mut panel, at(404), cmd::RAMCTRL, &[0x00, 0xF0, 0xFF]);
    assert_eq!(panel.ramctrl(), [0x00, 0xF0]);
}

#[test]
fn an_over_long_parameter_list_is_capped_and_counted() {
    // A `dc = 1` segment is guest controlled and may be 32 KiB, so the kept copy is capped at the
    // 14-byte gamma rows.
    let mut panel = lit_panel();
    assert_eq!(panel.dropped_param_bytes(), 0);

    let flood = vec![0xAAu8; 150_000];
    tx_param(&mut panel, at(400), cmd::NOP, &flood);
    tx_param(&mut panel, at(401), cmd::NOP, &flood);
    assert_eq!(
        panel.dropped_param_bytes(),
        2 * (150_000 - MAX_PARAMS) as u64
    );
    for entry in panel.trace() {
        assert!(entry.params.len() <= MAX_PARAMS, "{:?}", entry.cmd);
    }

    let gamma = [
        0xD0, 0x04, 0x08, 0x0A, 0x09, 0x05, 0x2D, 0x43, 0x49, 0x09, 0x16, 0x15, 0x26, 0x2B,
    ];
    assert_eq!(gamma.len(), MAX_PARAMS);
    tx_param(&mut panel, at(402), 0xE0, &gamma);
    assert_eq!(panel.stored_params(0xE0), Some(&gamma[..]));

    tx_param(&mut panel, at(403), 0xE1, &[0x77u8; 40]);
    assert_eq!(panel.stored_params(0xE1), Some(&[0x77u8; MAX_PARAMS][..]));
}

#[test]
fn publish_copies_only_the_dirty_rows_and_the_panel_flags() {
    // The span is a delta, and the port carries the raw view plus the flags for the other two.
    let mut panel = lit_panel();
    let mut backlight = Backlight::default();
    backlight.ledc(at(399), 0, 1023, 10, 5000);
    panel.set_backlight(&backlight);
    let mut port = FramePort::new();

    set_window(&mut panel, at(400), 0, PANEL_WIDTH - 1, 17, 18);
    tx_color(
        &mut panel,
        at(401),
        cmd::RAMWR,
        &[&pixels(0x145D, 2 * PANEL_WIDTH as usize)],
    );
    assert_eq!(panel.dirty_rows(), Some((17, 18)));

    assert_eq!(panel.publish(&mut port), Some((17, 18)));
    assert_eq!(port.dirty_rows(), Some((17, 18)));
    assert!(port.powered());
    assert!(!port.sleeping());
    assert!(port.inverted());
    assert_eq!(port.backlight(), 1023 << 4);
    assert_eq!(port.pixels()[17 * PANEL_WIDTH as usize], 0x145D);
    assert_eq!(port.pixels()[16 * PANEL_WIDTH as usize], 0);
    assert_eq!(port.pixels()[19 * PANEL_WIDTH as usize], 0);

    assert_eq!(panel.dirty_rows(), None);
    assert_eq!(panel.publish(&mut port), None);
    assert_eq!(
        port.dirty_rows(),
        Some((17, 18)),
        "the port keeps its span until its own reader takes it"
    );
    assert_eq!(port.take_dirty(), Some((17, 18)));
}

#[test]
fn publish_carries_dispon_and_the_invon_rule_so_a_host_can_derive_the_glass() {
    // The web renderer draws the glass from `FramePort`, so the port must say whether the glass is
    // memory's complement (`inverted != invon_shows_ram`) and whether DISPON is in force.
    const UI_SKY: u16 = 0x145D;
    let mut panel = lit_panel();
    let mut port = FramePort::new();
    set_window(&mut panel, at(400), 0, 0, 0, 0);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(UI_SKY, 1)]);
    panel.publish(&mut port);
    assert!(port.inverted() && port.invon_shows_ram() && port.display_on());
    assert!(!port.glass_complement());
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), UI_SKY);

    tx_param(&mut panel, at(402), cmd::INVOFF, &[]);
    panel.publish(&mut port);
    assert!(port.glass_complement());
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), !UI_SKY);

    panel.set_invon_shows_ram(false);
    panel.publish(&mut port);
    assert!(!port.invon_shows_ram());
    assert!(!port.glass_complement());
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), UI_SKY);

    tx_param(&mut panel, at(403), cmd::DISPOFF, &[]);
    panel.publish(&mut port);
    assert!(!port.display_on());
    assert_eq!(panel.pixel(FrameView::Glass, 0, 0), 0);
    tx_param(&mut panel, at(404), cmd::DISPON, &[]);
    panel.publish(&mut port);
    assert!(port.display_on());
}

#[test]
fn a_rail_reset_and_a_restore_repaint_the_whole_frame_port() {
    // No snapshot section carries the frame and `publish` copies dirty rows only, so both the rail
    // reset and a restore must mark the whole panel dirty or the host keeps the pre-reset image.
    let mut panel = lit_panel();
    let mut port = FramePort::new();
    set_window(&mut panel, at(400), 0, 0, 0, 0);
    tx_color(&mut panel, at(401), cmd::RAMWR, &[&pixels(0xABCD, 1)]);
    assert_eq!(panel.publish(&mut port), Some((0, 0)));
    assert_eq!(port.pixels()[0], 0xABCD);

    panel.reset(BoardDomain::BoardRail);
    assert_eq!(panel.raw()[0], 0);
    assert_eq!(
        panel.dirty_rows(),
        Some((0, PANEL_HEIGHT - 1)),
        "the cleared memory is a change the host has to see"
    );
    assert_eq!(panel.publish(&mut port), Some((0, PANEL_HEIGHT - 1)));
    assert_eq!(port.pixels()[0], 0);
    assert!(port.pixels().iter().all(|&pixel| pixel == 0));

    // The restore path: full memory with an empty dirty span repaints on demand.
    let mut restored = lit_panel();
    set_window(&mut restored, at(500), 0, 0, 0, 0);
    tx_color(&mut restored, at(501), cmd::RAMWR, &[&pixels(0x1234, 1)]);
    assert_eq!(restored.publish(&mut port), Some((0, 0)));
    assert_eq!(restored.dirty_rows(), None);
    assert_eq!(restored.publish(&mut port), None);

    let mut blank = FramePort::new();
    assert_eq!(blank.pixels()[0], 0);
    restored.mark_all_dirty();
    assert_eq!(restored.publish(&mut blank), Some((0, PANEL_HEIGHT - 1)));
    assert_eq!(blank.pixels()[0], 0x1234);
    assert_eq!(blank.dirty_rows(), Some((0, PANEL_HEIGHT - 1)));
}

#[test]
fn the_published_duty_is_the_resolved_brightness_not_the_bare_duty_register() {
    // `FramePort::backlight` must carry the resolved brightness (`sig_out_en`, `idle_lv` and
    // polarity applied): the bare `DUTY_R` is the exact inverse of `perceived` under active low.
    let mut port = FramePort::new();

    let mut panel = lit_panel();
    let mut low = Backlight::new(false);
    low.ledc(at(400), 0, 0, 10, 5000);
    panel.set_backlight(&low);
    assert_eq!(panel.brightness().percent(), 100);
    panel.publish(&mut port);
    assert_eq!(u32::from(port.backlight()) >> 4, 1024);

    low.ledc(at(401), 0, 1023, 10, 5000);
    panel.set_backlight(&low);
    assert_eq!(panel.brightness().percent(), 0);
    panel.publish(&mut port);
    assert_eq!(u32::from(port.backlight()) >> 4, 1);

    let mut high = Backlight::default();
    high.ledc(at(402), 0, 1023, 10, 5000);
    panel.set_backlight(&high);
    panel.publish(&mut port);
    assert_eq!(port.backlight(), 1023 << 4, "the BSP's 100 % is unchanged");
    high.set_enabled(false);
    panel.set_backlight(&high);
    assert!(panel.brightness().is_off());
    panel.publish(&mut port);
    assert_eq!(port.backlight(), 0);

    // A reprogrammed duty resolution stays on the ABI's 10-bit scale rather than clamping into a
    // `u16`.
    let mut wide = Backlight::default();
    wide.ledc(at(403), 0, 1u32 << 20, 20, 5000);
    panel.set_backlight(&wide);
    assert_eq!(panel.brightness().percent(), 100);
    panel.publish(&mut port);
    assert_eq!(u32::from(port.backlight()) >> 4, 1024);

    wide.ledc(at(404), 0, 1u32 << 19, 20, 5000);
    panel.set_backlight(&wide);
    assert_eq!(panel.brightness().percent(), 50);
    panel.publish(&mut port);
    assert_eq!(u32::from(port.backlight()) >> 4, 512);
}

#[test]
fn a_brightness_pair_never_panics_however_it_was_built() {
    // A pair restored from a snapshot outside `level <= scale` must not panic: restored state is
    // guest reachable.
    let huge = Brightness::new(u32::MAX, 20);
    assert_eq!(huge.level(), huge.scale());
    assert_eq!(huge.scale_value(u32::MAX), u32::MAX);
    assert_eq!(huge.with_polarity(false).level(), 0);
    assert_eq!(huge.percent(), 100);
    assert_eq!(huge.frame_duty(), 1024 << 4);

    assert_eq!(Brightness::new(16, 0).scale(), 2);
    assert_eq!(Brightness::new(16, 255).scale(), 1 << 20);
    assert!(Brightness::OFF.is_off());
    assert_eq!(Brightness::OFF.frame_duty(), 0);
}

const BOOT_SPEC: &str = include_str!("../../../specs/st7789-boot.toml");

#[derive(Clone, Default, PartialEq, Eq, Debug)]
struct SpecStep {
    index: u8,
    name: String,
    cmd: u8,
    params: Vec<u8>,
    delay_ms_after: u32,
}

fn spec_int(text: &str) -> u32 {
    let text = text.trim();
    match text.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16).expect("hex integer"),
        None => text.parse().expect("decimal integer"),
    }
}

/// The `[[step]]` rows of `specs/st7789-boot.toml`. A minimal reader: `pemu-board` may depend on
/// serde only, and a dev-dependency counts (`cargo xtask layering`, rule `third-party`).
fn parse_boot_spec(text: &str) -> Vec<SpecStep> {
    let mut steps: Vec<SpecStep> = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "[[step]]" {
            steps.push(SpecStep::default());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, mut value) = (key.trim(), value.trim().to_string());
        if value.starts_with('[') && !value.contains(']') {
            for more in lines.by_ref() {
                value.push_str(more.trim());
                if more.contains(']') {
                    break;
                }
            }
        }
        let Some(step) = steps.last_mut() else {
            continue;
        };
        match key {
            "index" => step.index = spec_int(&value) as u8,
            "name" => step.name = value.trim_matches('"').to_string(),
            "cmd" => step.cmd = spec_int(&value) as u8,
            "params" => {
                step.params = value
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(str::trim)
                    .filter(|part| !part.is_empty())
                    .map(|part| spec_int(part) as u8)
                    .collect();
            }
            "delay_ms_after" => step.delay_ms_after = spec_int(&value),
            _ => {}
        }
    }
    steps
}

#[test]
fn boot_spec_matches_the_table_in_the_model() {
    let spec = parse_boot_spec(BOOT_SPEC);
    assert_eq!(spec.len(), 23, "the boot sequence has 23 steps");
    assert_eq!(spec.len(), BOOT_SEQUENCE.len());
    for (row, step) in spec.iter().zip(BOOT_SEQUENCE) {
        let mirrored = SpecStep {
            index: step.index,
            name: step.name.to_string(),
            cmd: step.cmd,
            params: step.params.to_vec(),
            delay_ms_after: step.delay_ms_after,
        };
        assert_eq!(*row, mirrored, "specs/st7789-boot.toml row {}", row.index);
    }
    for (at, row) in spec.iter().enumerate() {
        assert_eq!(row.index as usize, at + 1);
    }
}

/// Replays one boot step the way the driver frames it.
fn send_boot_step(panel: &mut St7789p3, t: VTime, step: &BootStep) {
    tx_param(panel, t, step.cmd, step.params);
}

#[test]
fn the_boot_sequence_leaves_the_panel_ready_and_matches_the_spec() {
    let mut panel = St7789p3::new(PanelConfig {
        invon_shows_ram: true,
    });
    panel.set_powered(true);
    let mut now = 0u64;
    for step in BOOT_SEQUENCE {
        send_boot_step(&mut panel, at(now), step);
        now += u64::from(step.delay_ms_after);
    }

    assert_eq!(panel.boot_mismatch(), None);
    assert_eq!(panel.commands(), 23);
    // 16 bpp, MADCTL 0x00, INVON, DISPON, out of sleep, and RAMCTRL big-endian.
    assert_eq!(panel.colmod(), 0x55);
    assert_eq!(panel.madctl(), 0x00);
    assert_eq!(panel.ramctrl(), [0x00, 0xF0]);
    assert!(panel.inverted());
    assert!(panel.display_on());
    assert!(!panel.sleeping());
    assert!(panel.lit());
    // The driver's 20 ms SWRESET-to-SLPOUT gap breaks the 120 ms rule on every real boot; the lint
    // says so once and blocks nothing.
    assert_eq!(panel.warnings().len(), 1, "{:?}", panel.warnings());
    assert_eq!(panel.warnings()[0].lint, TimingLint::SleepCycleTooFast);
    assert_eq!(panel.warnings()[0].cmd, cmd::SLPOUT);
    assert_eq!(panel.warnings()[0].at, at(20));
    // The vendor rows were stored, the last 0xD0 winning.
    assert_eq!(panel.stored_params(0xD0), Some(&[0xA4, 0xA1][..]));
    assert_eq!(panel.stored_params(0xD6), Some(&[0xA1][..]));
}

#[test]
fn a_boot_sequence_that_drops_a_step_is_reported_with_its_row() {
    let mut panel = St7789p3::default();
    panel.set_powered(true);
    for step in BOOT_SEQUENCE.iter().filter(|step| step.index != 4) {
        send_boot_step(&mut panel, at(1000 * u64::from(step.index)), step);
    }
    let mismatch = panel.boot_mismatch().expect("the missing COLMOD is caught");
    assert_eq!(mismatch.step, 4);
    assert_eq!(mismatch.expected.expect("expected step").cmd, cmd::COLMOD);
    assert_eq!(mismatch.found.expect("found command").cmd, 0xB0);

    let mut short = St7789p3::default();
    short.set_powered(true);
    for step in &BOOT_SEQUENCE[..2] {
        send_boot_step(&mut short, at(1000 * u64::from(step.index)), step);
    }
    let mismatch = short.boot_mismatch().expect("a short trace is a mismatch");
    assert_eq!(mismatch.step, 3);
    assert_eq!(mismatch.found, None);
}

/// FNV-1a over the raw frame's wire bytes: the whole-frame half of the golden; the pixel counts
/// and corner pixels say it is the right frame.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The raw frame as it goes over the wire: RGB565, big-endian, row-major.
fn raw_bytes(panel: &St7789p3) -> Vec<u8> {
    let mut out = Vec::with_capacity(PANEL_PIXELS * 2);
    for &pixel in panel.raw() {
        out.extend_from_slice(&pixel.to_be_bytes());
    }
    out
}

/// Fills one rectangle as an LVGL flush does: CASET, RASET, RAMWR (`draw_bitmap` sends
/// inclusive ends).
fn flush_rect(panel: &mut St7789p3, t: VTime, x: u16, y: u16, w: u16, h: u16, colour: u16) {
    set_window(panel, t, x, x + w - 1, y, y + h - 1);
    let body = pixels(colour, w as usize * h as usize);
    tx_color(panel, t, cmd::RAMWR, &[&body]);
}

#[test]
fn raw_frame_golden_bytes() {
    // The measured boot menu of `specs/notes/g3-behavior.md`: background, title plate fill at
    // (8, 11, 145, 27) and the selected Display card fill at (15, 56, 94, 32).
    const UI_SKY: u16 = 0x145D;
    const UI_PAPER: u16 = 0xF7BD;
    const UI_YELLOW: u16 = 0xFEC5;

    let mut panel = lit_panel();
    flush_rect(&mut panel, at(400), 0, 0, PANEL_WIDTH, PANEL_HEIGHT, UI_SKY);
    flush_rect(&mut panel, at(401), 8, 11, 145, 27, UI_PAPER);
    flush_rect(&mut panel, at(402), 15, 56, 94, 32, UI_YELLOW);

    let count = |colour: u16| panel.raw().iter().filter(|&&p| p == colour).count();
    assert_eq!(count(UI_PAPER), 145 * 27);
    assert_eq!(count(UI_YELLOW), 94 * 32);
    assert_eq!(count(UI_SKY), PANEL_PIXELS - 145 * 27 - 94 * 32);

    assert_eq!(raw_at(&panel, 8, 11), UI_PAPER);
    assert_eq!(raw_at(&panel, 152, 37), UI_PAPER);
    assert_eq!(raw_at(&panel, 153, 37), UI_SKY);
    assert_eq!(raw_at(&panel, 152, 38), UI_SKY);
    assert_eq!(raw_at(&panel, 15, 56), UI_YELLOW);
    assert_eq!(raw_at(&panel, 108, 87), UI_YELLOW);
    assert_eq!(raw_at(&panel, 109, 87), UI_SKY);

    let bytes = raw_bytes(&panel);
    assert_eq!(bytes.len(), PANEL_PIXELS * 2);
    assert_eq!(&bytes[..4], &[0x14, 0x5D, 0x14, 0x5D]);
    // The plate starts at x = 8, so on row 11 the background ends at byte 2 * 8.
    let row11 = 11 * PANEL_WIDTH as usize * 2;
    assert_eq!(
        &bytes[row11 + 12..row11 + 20],
        &[0x14, 0x5D, 0x14, 0x5D, 0xF7, 0xBD, 0xF7, 0xBD]
    );
    assert_eq!(fnv1a64(&bytes), 0x1bf7_9986_3122_0f26);
}
