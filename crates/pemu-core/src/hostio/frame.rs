//! The frame port of `HostIo::frame`: the raw panel frame and the flags a host needs to show it.

use core::fmt;

pub const FRAME_WIDTH: usize = 240;

pub const FRAME_HEIGHT: usize = 320;

/// Frame port of `HostIo::frame`: the raw RGB565 240x320 frame, the dirty row span, the frame
/// generation, the backlight duty, the panel flags, and the board's `invon_shows_ram` rule a host
/// needs to turn the raw view into the glass.
///
/// The pixel buffer never reallocates. The frame is output, so no snapshot section carries it:
/// after a restore the panel model repaints through [`FramePort::pixels_mut`]. The panel model
/// owns the reset state, so the defaults of [`FramePort::new`] are UNVERIFIED.
#[derive(Clone, PartialEq, Eq)]
pub struct FramePort {
    pixels: Box<[u16]>,
    dirty: Option<(u16, u16)>,
    generation: u64,
    backlight: u16,
    powered: bool,
    sleeping: bool,
    inverted: bool,
    display_on: bool,
    invon_shows_ram: bool,
}

impl Default for FramePort {
    fn default() -> Self {
        FramePort::new()
    }
}

impl fmt::Debug for FramePort {
    /// Without the pixels: 240x320 of them say nothing in a test failure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FramePort")
            .field("dirty", &self.dirty)
            .field("generation", &self.generation)
            .field("backlight", &self.backlight)
            .field("powered", &self.powered)
            .field("sleeping", &self.sleeping)
            .field("inverted", &self.inverted)
            .field("display_on", &self.display_on)
            .field("invon_shows_ram", &self.invon_shows_ram)
            .finish_non_exhaustive()
    }
}

impl FramePort {
    /// A blank port: zeroed pixels, no dirty row, generation 0, backlight 0, panel off, sleeping,
    /// not inverted, DISPOFF, and the board default `invon_shows_ram = true`.
    pub fn new() -> Self {
        FramePort {
            pixels: vec![0; FRAME_WIDTH * FRAME_HEIGHT].into_boxed_slice(),
            dirty: None,
            generation: 0,
            backlight: 0,
            powered: false,
            sleeping: true,
            inverted: false,
            display_on: false,
            invon_shows_ram: true,
        }
    }

    pub fn width(&self) -> usize {
        FRAME_WIDTH
    }

    pub fn height(&self) -> usize {
        FRAME_HEIGHT
    }

    /// Row-major RGB565.
    pub fn pixels(&self) -> &[u16] {
        &self.pixels
    }

    /// The writer marks the rows it touched with [`FramePort::mark_dirty`]; a panel write spans an
    /// arbitrary window, so this method cannot.
    pub fn pixels_mut(&mut self) -> &mut [u16] {
        &mut self.pixels
    }

    /// Address of the pixel buffer; it never changes for the life of the port.
    pub fn as_ptr(&self) -> *const u16 {
        self.pixels.as_ptr()
    }

    /// Adds rows `first..=last` to the dirty span, clamped to the panel. An empty range or a
    /// `first` beyond the last row changes nothing.
    pub fn mark_dirty(&mut self, first: u16, last: u16) {
        let max = (FRAME_HEIGHT - 1) as u16;
        if first > last || first > max {
            return;
        }
        let (first, last) = (first, last.min(max));
        self.dirty = Some(match self.dirty {
            None => (first, last),
            Some((f, l)) => (f.min(first), l.max(last)),
        });
    }

    /// `None` when no row changed since the span was last taken.
    pub fn dirty_rows(&self) -> Option<(u16, u16)> {
        self.dirty
    }

    pub fn take_dirty(&mut self) -> Option<(u16, u16)> {
        self.dirty.take()
    }

    /// Frames completed: the generation of the frame now in the buffer.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Puts back the generation a snapshot recorded after a restore repainted the pixels, and adds
    /// the recorded dirty span: a host sees the saved machine's frame numbering and still redraws
    /// the whole screen, because what it showed before the restore is not what the port holds.
    pub fn restore_progress(&mut self, generation: u64, dirty: Option<(u16, u16)>) {
        self.generation = generation;
        if let Some((first, last)) = dirty {
            self.mark_dirty(first, last);
        }
    }

    /// Counts one completed frame and returns the new generation, the `arg` of an
    /// [`EventKind::Frame`](super::EventKind::Frame) event.
    pub fn present(&mut self) -> u64 {
        self.generation += 1;
        self.generation
    }

    /// Resolved backlight brightness (output enable and polarity applied) on the 10-bit LEDC scale
    /// with four fractional bits: `1024 << 4` is full, 0 is dark
    /// (`pemu_board::backlight::Brightness::frame_duty`).
    pub fn backlight(&self) -> u16 {
        self.backlight
    }

    pub fn set_backlight(&mut self, duty: u16) {
        self.backlight = duty;
    }

    pub fn powered(&self) -> bool {
        self.powered
    }

    pub fn set_powered(&mut self, on: bool) {
        self.powered = on;
    }

    /// Sleep-in mode.
    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    pub fn set_sleeping(&mut self, sleeping: bool) {
        self.sleeping = sleeping;
    }

    /// ST7789 `INVON`.
    pub fn inverted(&self) -> bool {
        self.inverted
    }

    pub fn set_inverted(&mut self, inverted: bool) {
        self.inverted = inverted;
    }

    /// DISPOFF blanks the glass and keeps panel memory.
    pub fn display_on(&self) -> bool {
        self.display_on
    }

    pub fn set_display_on(&mut self, on: bool) {
        self.display_on = on;
    }

    /// With it on, the glass shows panel memory unmodified while INVON is in force and its
    /// complement otherwise; with it off the two are exchanged. The raw view never depends on it.
    pub fn invon_shows_ram(&self) -> bool {
        self.invon_shows_ram
    }

    pub fn set_invon_shows_ram(&mut self, shows_ram: bool) {
        self.invon_shows_ram = shows_ram;
    }

    /// Whether the glass shows the bitwise complement of panel memory:
    /// `inverted != invon_shows_ram`.
    pub fn glass_complement(&self) -> bool {
        self.inverted != self.invon_shows_ram
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_port_tracks_dirty_rows_generation_and_panel_flags() {
        let mut frame = FramePort::new();
        let ptr = frame.as_ptr();
        assert_eq!((frame.width(), frame.height()), (240, 320));
        assert!(frame.sleeping() && !frame.powered() && !frame.inverted());
        assert!(!frame.display_on() && frame.invon_shows_ram() && frame.glass_complement());
        for (inverted, shows_ram, complement) in [
            (true, true, false),
            (false, true, true),
            (true, false, true),
            (false, false, false),
        ] {
            frame.set_inverted(inverted);
            frame.set_invon_shows_ram(shows_ram);
            assert_eq!(
                frame.glass_complement(),
                complement,
                "{inverted} {shows_ram}"
            );
        }
        frame.set_inverted(false);
        frame.set_invon_shows_ram(true);
        frame.set_display_on(true);
        assert!(frame.display_on());
        frame.set_display_on(false);

        // A window write marks its rows; spans merge and clamp to the panel.
        let row = 17;
        frame.pixels_mut()[row * FRAME_WIDTH..row * FRAME_WIDTH + 3].fill(0xf800);
        frame.mark_dirty(row as u16, row as u16);
        assert_eq!(frame.dirty_rows(), Some((17, 17)));
        frame.mark_dirty(2, 4);
        frame.mark_dirty(300, 400);
        assert_eq!(frame.dirty_rows(), Some((2, 319)));
        frame.mark_dirty(9, 8);
        frame.mark_dirty(320, 320);
        assert_eq!(frame.take_dirty(), Some((2, 319)));
        assert_eq!(frame.take_dirty(), None);
        assert_eq!(frame.pixels()[row * FRAME_WIDTH + 2], 0xf800);

        assert_eq!((frame.present(), frame.present()), (1, 2));
        assert_eq!(frame.generation(), 2);
        frame.set_backlight(4095);
        frame.set_powered(true);
        frame.set_sleeping(false);
        frame.set_inverted(true);
        assert_eq!(frame.backlight(), 4095);
        assert!(frame.powered() && !frame.sleeping() && frame.inverted());
        assert_eq!(frame.as_ptr(), ptr);
    }
}
