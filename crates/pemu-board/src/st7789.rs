//! The ST7789P3 panel: command decode over SPI2, 240x320 RGB565 panel memory, the three frame
//! views, the timing lint and the boot-sequence trace (ST7789 datasheet, `specs/st7789-boot.toml`,
//! `specs/notes/g3-behavior.md`).
//!
//! - A `dc = 0` transaction carries one command byte; `dc = 1` carries the parameters or pixels of
//!   the command in force. Pixels are RGB565, most significant byte first (RAMCTRL ENDIAN clear).
//! - RAMWR resets the pixel pointer to the window origin; it walks left to right, top to bottom,
//!   and wraps inside the window. SWRESET keeps frame memory, MADCTL and COLMOD.
//! - The firmware sends INVON to get the colours it drew, so INVON shows panel memory unmodified
//!   and INVOFF its complement; a switch, because it is UNVERIFIED on real glass
//!   (g3-menu-colours-invon).
//!
//! Panel memory is heap-allocated: a large stack array overflows the 1 MiB Windows main stack.

use pemu_core::hostio::FramePort;
use pemu_core::sched::ChipId;
use pemu_core::time::VTime;
use serde::{Deserialize, Serialize};

use crate::backlight::{Backlight, Brightness};
use crate::traits::{BoardDomain, Chip, SpiDevice};

/// Columns of panel memory (the frame memory is 240 columns by 320 rows).
pub const PANEL_WIDTH: u16 = 240;

pub const PANEL_HEIGHT: u16 = 320;

pub const PANEL_PIXELS: usize = PANEL_WIDTH as usize * PANEL_HEIGHT as usize;

/// Command bytes the model decodes.
pub mod cmd {
    pub const NOP: u8 = 0x00;
    pub const SWRESET: u8 = 0x01;
    pub const SLPIN: u8 = 0x10;
    pub const SLPOUT: u8 = 0x11;
    pub const INVOFF: u8 = 0x20;
    pub const INVON: u8 = 0x21;
    pub const DISPOFF: u8 = 0x28;
    pub const DISPON: u8 = 0x29;
    pub const CASET: u8 = 0x2A;
    pub const RASET: u8 = 0x2B;
    /// Memory write; resets the pixel pointer to the window origin.
    pub const RAMWR: u8 = 0x2C;
    pub const MADCTL: u8 = 0x36;
    pub const COLMOD: u8 = 0x3A;
    /// Memory write continue; keeps the pixel pointer.
    pub const RAMWRC: u8 = 0x3C;
    /// RAM control: the ENDIAN bit lives here.
    pub const RAMCTRL: u8 = 0xB0;
}

/// MADCTL bit MY: row address order, mirroring the rows.
pub const MADCTL_MY: u8 = 0x80;
/// MADCTL bit MX: column address order, mirroring the columns.
pub const MADCTL_MX: u8 = 0x40;
/// MADCTL bit MV: row and column exchange, which swaps the axes.
pub const MADCTL_MV: u8 = 0x20;
/// MADCTL bit RGB/BGR. Stored only: no view re-orders the elements.
pub const MADCTL_BGR: u8 = 0x08;

/// COLMOD value for 16 bits per pixel, the only pixel format this model stores.
pub const COLMOD_16BPP: u8 = 0x55;

/// COLMOD reset value: 18 bits per pixel.
pub const COLMOD_RESET: u8 = 0x66;

/// Commands whose parameters are stored uninterpreted (gamma too: the model renders memory, not
/// the gamma curve). 0xD6 is P3 specific, meaning UNVERIFIED.
pub const STORED_COMMANDS: &[u8] = &[
    0xB2, 0xB7, 0xBB, 0xC0, 0xC2, 0xC3, 0xC4, 0xC6, 0xD0, 0xD6, 0xE0, 0xE1,
];

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BootStep {
    pub index: u8,
    pub name: &'static str,
    pub cmd: u8,
    pub params: &'static [u8],
    /// The driver's own delay after the command, in milliseconds. Never enforced.
    pub delay_ms_after: u32,
}

/// The 23 commands the official firmware sends between power-on and the first flush, row for row
/// as `specs/st7789-boot.toml` (`tests/st7789.rs::boot_spec_matches_table` keeps them in step).
pub const BOOT_SEQUENCE: &[BootStep] = &[
    BootStep {
        index: 1,
        name: "SWRESET",
        cmd: 0x01,
        params: &[],
        delay_ms_after: 20,
    },
    BootStep {
        index: 2,
        name: "SLPOUT",
        cmd: 0x11,
        params: &[],
        delay_ms_after: 100,
    },
    BootStep {
        index: 3,
        name: "MADCTL",
        cmd: 0x36,
        params: &[0x00],
        delay_ms_after: 0,
    },
    BootStep {
        index: 4,
        name: "COLMOD",
        cmd: 0x3A,
        params: &[0x55],
        delay_ms_after: 0,
    },
    BootStep {
        index: 5,
        name: "RAMCTRL",
        cmd: 0xB0,
        params: &[0x00, 0xF0],
        delay_ms_after: 0,
    },
    BootStep {
        index: 6,
        name: "PORCTRL",
        cmd: 0xB2,
        params: &[0x05, 0x05, 0x00, 0x33, 0x33],
        delay_ms_after: 0,
    },
    BootStep {
        index: 7,
        name: "GCTRL",
        cmd: 0xB7,
        params: &[0x35],
        delay_ms_after: 0,
    },
    BootStep {
        index: 8,
        name: "VCOMS",
        cmd: 0xBB,
        params: &[0x21],
        delay_ms_after: 0,
    },
    BootStep {
        index: 9,
        name: "LCMCTRL",
        cmd: 0xC0,
        params: &[0x2C],
        delay_ms_after: 0,
    },
    BootStep {
        index: 10,
        name: "VDVVRHEN",
        cmd: 0xC2,
        params: &[0x01],
        delay_ms_after: 0,
    },
    BootStep {
        index: 11,
        name: "VRHS",
        cmd: 0xC3,
        params: &[0x0B],
        delay_ms_after: 0,
    },
    BootStep {
        index: 12,
        name: "VDVS",
        cmd: 0xC4,
        params: &[0x20],
        delay_ms_after: 0,
    },
    BootStep {
        index: 13,
        name: "FRCTRL2",
        cmd: 0xC6,
        params: &[0x0F],
        delay_ms_after: 0,
    },
    BootStep {
        index: 14,
        name: "PWCTRL1",
        cmd: 0xD0,
        params: &[0xA7, 0xA1],
        delay_ms_after: 0,
    },
    BootStep {
        index: 15,
        name: "PWCTRL1",
        cmd: 0xD0,
        params: &[0xA4, 0xA1],
        delay_ms_after: 0,
    },
    BootStep {
        index: 16,
        name: "VENDOR_D6",
        cmd: 0xD6,
        params: &[0xA1],
        delay_ms_after: 0,
    },
    BootStep {
        index: 17,
        name: "PVGAMCTRL",
        cmd: 0xE0,
        params: &[
            0xD0, 0x04, 0x08, 0x0A, 0x09, 0x05, 0x2D, 0x43, 0x49, 0x09, 0x16, 0x15, 0x26, 0x2B,
        ],
        delay_ms_after: 0,
    },
    BootStep {
        index: 18,
        name: "NVGAMCTRL",
        cmd: 0xE1,
        params: &[
            0xD0, 0x03, 0x09, 0x0A, 0x0A, 0x06, 0x2E, 0x44, 0x40, 0x3A, 0x15, 0x15, 0x26, 0x2A,
        ],
        delay_ms_after: 10,
    },
    BootStep {
        index: 19,
        name: "INVON",
        cmd: 0x21,
        params: &[],
        delay_ms_after: 0,
    },
    BootStep {
        index: 20,
        name: "MADCTL",
        cmd: 0x36,
        params: &[0x00],
        delay_ms_after: 0,
    },
    BootStep {
        index: 21,
        name: "DISPON",
        cmd: 0x29,
        params: &[],
        delay_ms_after: 0,
    },
    BootStep {
        index: 22,
        name: "MADCTL",
        cmd: 0x36,
        params: &[0x00],
        delay_ms_after: 0,
    },
    BootStep {
        index: 23,
        name: "MADCTL",
        cmd: 0x36,
        params: &[0x00],
        delay_ms_after: 0,
    },
];

/// Which of the three views `display.frame(kind)` renders.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum FrameView {
    /// Panel memory, exact RGB565. Used for bit-exact goldens.
    Raw,
    /// Black unless powered, out of sleep and DISPON; otherwise memory with inversion applied per
    /// `invon_shows_ram`. Used by the UI.
    Glass,
    /// `Glass` scaled by the backlight brightness.
    Perceived,
}

/// A timing rule the guest broke; warnings only (class B).
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TimingLint {
    /// A command arrived less than 5 ms after SLPOUT.
    CommandAfterSlpout,
    /// SLPIN and SLPOUT were less than 120 ms apart.
    SleepCycleTooFast,
    /// RAMWR or RAMWRC arrived while COLMOD was not [`COLMOD_16BPP`] (including before any COLMOD),
    /// so the bytes are decoded as RGB565 although they are not. 18 bpp is unmodeled (Fidelity::U).
    RamwrWrongColmod,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TimingWarning {
    pub lint: TimingLint,
    pub cmd: u8,
    pub at: VTime,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TraceEntry {
    pub cmd: u8,
    /// The parameter bytes that followed it. Empty for RAMWR and RAMWRC, whose data is pixels.
    pub params: Vec<u8>,
    /// Pixel bytes that followed a RAMWR or RAMWRC. Counted, not kept: a flush is up to 32768.
    pub pixel_bytes: u64,
    pub at: VTime,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BootMismatch {
    /// 1-based boot step that differs, or the number of steps when the trace is short.
    pub step: u8,
    pub expected: Option<BootStep>,
    /// What the run sent, absent when the trace ended early.
    pub found: Option<TraceEntry>,
}

impl core::fmt::Display for BootMismatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "boot step {}: expected ", self.step)?;
        match &self.expected {
            Some(step) => write!(f, "{} {:#04x} {:02x?}", step.name, step.cmd, step.params)?,
            None => write!(f, "no further command")?,
        }
        write!(f, ", found ")?;
        match &self.found {
            Some(entry) => write!(f, "{:#04x} {:02x?}", entry.cmd, entry.params),
            None => write!(f, "end of trace"),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct PanelConfig {
    /// Whether INVON shows memory unmodified (INVOFF the complement); UNVERIFIED on real glass.
    pub invon_shows_ram: bool,
}

impl Default for PanelConfig {
    fn default() -> Self {
        PanelConfig {
            invon_shows_ram: true,
        }
    }
}

/// The longest parameter list decoded or sent by the boot table (the 14-byte gamma rows). Excess
/// guest bytes are counted and dropped, so neither `params` nor a trace entry can grow.
pub const MAX_PARAMS: usize = 14;

/// Commands kept in the trace: the boot plus the first flushes. The trace is not guest state.
pub const TRACE_LIMIT: usize = 128;

const SLPOUT_SETTLE: VTime = VTime::from_ms(5);

const SLEEP_CYCLE: VTime = VTime::from_ms(120);

/// The ST7789P3 LCD controller. Panel memory is the frame memory in physical layout, row-major:
/// MADCTL is applied where the pixel pointer addresses memory, not when a view is rendered.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct St7789p3 {
    cfg: PanelConfig,
    #[serde(deserialize_with = "pemu_core::snap::exact_boxed::<_, _, PANEL_PIXELS>")]
    ram: Box<[u16]>,
    powered: bool,
    sleeping: bool,
    display_on: bool,
    inverted: bool,
    madctl: u8,
    colmod: u8,
    ramctrl: [u8; 2],
    xs: u16,
    xe: u16,
    ys: u16,
    ye: u16,
    x: u16,
    y: u16,
    carry: Option<u8>,
    in_pixels: bool,
    cmd: Option<u8>,
    params: Vec<u8>,
    applied: bool,
    dropped_params: u64,
    stored: Vec<(u8, Vec<u8>)>,
    brightness: Brightness,
    frame_duty: u16,
    trace: Vec<TraceEntry>,
    trace_cur: Option<usize>,
    commands: u64,
    ramwr_count: u64,
    pixels_written: u64,
    /// Host pacing: rows the next [`St7789p3::publish`] copies. Not serialized, so `state_hash`
    /// does not depend on when a host published.
    #[serde(skip)]
    dirty: Option<(u16, u16)>,
    warnings: Vec<TimingWarning>,
    last_slpout: Option<VTime>,
    last_slpin: Option<VTime>,
}

impl St7789p3 {
    /// Chip id of the panel, from the controller's part number. UNVERIFIED allocation.
    pub const CHIP_ID: ChipId = ChipId(0x7789);

    /// A panel as it comes up: sleeping, display off, inversion off, MADCTL 0x00, COLMOD 18 bpp,
    /// full window. Zeroed memory is the content policy (class C), not a measured pattern.
    pub fn new(cfg: PanelConfig) -> St7789p3 {
        St7789p3 {
            cfg,
            ram: vec![0u16; PANEL_PIXELS].into_boxed_slice(),
            powered: false,
            sleeping: true,
            display_on: false,
            inverted: false,
            madctl: 0x00,
            colmod: COLMOD_RESET,
            ramctrl: [0x00, 0x00],
            xs: 0,
            xe: PANEL_WIDTH - 1,
            ys: 0,
            ye: PANEL_HEIGHT - 1,
            x: 0,
            y: 0,
            carry: None,
            in_pixels: false,
            cmd: None,
            params: Vec::new(),
            applied: false,
            dropped_params: 0,
            stored: Vec::new(),
            brightness: Brightness::OFF,
            frame_duty: 0,
            trace: Vec::new(),
            trace_cur: None,
            commands: 0,
            ramwr_count: 0,
            pixels_written: 0,
            dirty: None,
            warnings: Vec::new(),
            last_slpout: None,
            // The panel powers up in sleep-in, so the 120 ms rule is already counting at t = 0;
            // otherwise the driver's 20 ms SWRESET-to-SLPOUT gap would raise nothing.
            last_slpin: Some(VTime(0)),
        }
    }

    pub fn config(&self) -> PanelConfig {
        self.cfg
    }

    /// Switches `invon_shows_ram` at run time; panel memory is unaffected.
    pub fn set_invon_shows_ram(&mut self, shows_ram: bool) {
        self.cfg.invon_shows_ram = shows_ram;
    }

    pub fn powered(&self) -> bool {
        self.powered
    }

    /// Raises or drops the panel rail; memory is cleared only by a `BoardRail` reset.
    pub fn set_powered(&mut self, on: bool) {
        self.powered = on;
    }

    pub fn sleeping(&self) -> bool {
        self.sleeping
    }

    /// Whether DISPON is in force (DISPOFF blanks the glass and keeps RAM).
    pub fn display_on(&self) -> bool {
        self.display_on
    }

    pub fn inverted(&self) -> bool {
        self.inverted
    }

    pub fn madctl(&self) -> u8 {
        self.madctl
    }

    /// The COLMOD value in force; 0x55 is 16 bpp.
    pub fn colmod(&self) -> u8 {
        self.colmod
    }

    /// The two RAMCTRL parameters in force; `00 F0` is MSB-first RGB565.
    pub fn ramctrl(&self) -> [u8; 2] {
        self.ramctrl
    }

    /// The address window `(xs, xe, ys, ye)` set by CASET and RASET, inclusive.
    pub fn window(&self) -> (u16, u16, u16, u16) {
        (self.xs, self.xe, self.ys, self.ye)
    }

    /// The pixel pointer, in window coordinates before the MADCTL transform.
    pub fn cursor(&self) -> (u16, u16) {
        (self.x, self.y)
    }

    /// The odd pixel byte held over to the next `dc = 1` segment, if any.
    pub fn carry(&self) -> Option<u8> {
        self.carry
    }

    pub fn stored_params(&self, cmd: u8) -> Option<&[u8]> {
        self.stored
            .iter()
            .find(|(c, _)| *c == cmd)
            .map(|(_, bytes)| bytes.as_slice())
    }

    pub fn commands(&self) -> u64 {
        self.commands
    }

    /// RAMWR commands decoded since the last reset (40 up to the settled boot menu).
    pub fn ramwr_count(&self) -> u64 {
        self.ramwr_count
    }

    pub fn pixels_written(&self) -> u64 {
        self.pixels_written
    }

    pub fn trace(&self) -> &[TraceEntry] {
        &self.trace
    }

    /// Parameter bytes dropped past [`MAX_PARAMS`]; only a guest that overruns a command gets here.
    pub fn dropped_param_bytes(&self) -> u64 {
        self.dropped_params
    }

    pub fn warnings(&self) -> &[TimingWarning] {
        &self.warnings
    }

    /// Returns the timing-lint warnings and clears them.
    pub fn take_warnings(&mut self) -> Vec<TimingWarning> {
        core::mem::take(&mut self.warnings)
    }
}

impl St7789p3 {
    /// One SPI2 user transaction. `dc = 0` is one command byte and ends any pixel write; `dc = 1`
    /// is parameters or pixels. `cs_release` drops an incomplete parameter list and an odd pixel
    /// byte, but the pixel pointer survives for a later RAMWRC (UNVERIFIED; the driver never
    /// releases CS inside a pixel run). Data with no command in force is dropped.
    pub fn transfer(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool) {
        if dc {
            self.data(t, data);
        } else {
            for &byte in data {
                self.begin_command(t, byte);
            }
        }
        if cs_release {
            self.cs_high();
        }
    }

    /// Starts a command context. Zero-parameter commands apply here.
    fn begin_command(&mut self, t: VTime, c: u8) {
        self.lint(t, c);
        self.commands += 1;
        self.params.clear();
        self.applied = false;
        self.cmd = Some(c);
        match c {
            cmd::RAMWR => {
                // RAMWR resets the pointer to (XS, YS) and drops any old carry.
                self.x = self.xs;
                self.y = self.ys;
                self.carry = None;
                self.in_pixels = true;
                self.ramwr_count += 1;
            }
            cmd::RAMWRC => {
                // Memory write continue: the pointer and the odd-byte carry both survive.
                self.in_pixels = true;
            }
            _ => {
                self.in_pixels = false;
                self.carry = None;
            }
        }
        self.trace_begin(c, t);
        match c {
            cmd::SWRESET => self.swreset(t),
            cmd::SLPIN => {
                self.sleeping = true;
                self.last_slpin = Some(t);
            }
            cmd::SLPOUT => {
                self.sleeping = false;
                self.last_slpout = Some(t);
            }
            cmd::INVOFF => self.inverted = false,
            cmd::INVON => self.inverted = true,
            cmd::DISPOFF => self.display_on = false,
            cmd::DISPON => self.display_on = true,
            _ => {}
        }
    }

    fn data(&mut self, t: VTime, bytes: &[u8]) {
        let _ = t;
        if self.in_pixels {
            if let Some(index) = self.trace_cur {
                self.trace[index].pixel_bytes += bytes.len() as u64;
            }
            self.write_pixels(bytes);
            return;
        }
        let Some(c) = self.cmd else { return };
        // Excess parameter bytes are counted and dropped so a guest cannot grow a snapshot.
        let taken = bytes.len().min(MAX_PARAMS - self.params.len());
        self.params.extend_from_slice(&bytes[..taken]);
        self.dropped_params += (bytes.len() - taken) as u64;
        if let Some(index) = self.trace_cur {
            self.trace[index].params.extend_from_slice(&bytes[..taken]);
        }
        self.apply_params(c);
    }

    fn cs_high(&mut self) {
        self.cmd = None;
        self.params.clear();
        self.applied = false;
        self.in_pixels = false;
        self.carry = None;
        self.trace_cur = None;
    }

    /// Applies a parameter command when its parameter count is first reached or passed: `dc = 1`
    /// framing carries no meaning, so 4 + 4 and 8 must decode alike. `applied` prevents a second
    /// application.
    fn apply_params(&mut self, c: u8) {
        let n = self.params.len();
        match c {
            cmd::CASET if n >= 4 && !self.applied => {
                let (a, b) = (be16(&self.params[0..2]), be16(&self.params[2..4]));
                // The range follows MV; values past the end clip.
                self.xs = a.min(self.col_max());
                self.xe = b.min(self.col_max());
                self.applied = true;
            }
            cmd::RASET if n >= 4 && !self.applied => {
                let (a, b) = (be16(&self.params[0..2]), be16(&self.params[2..4]));
                self.ys = a.min(self.row_max());
                self.ye = b.min(self.row_max());
                self.applied = true;
            }
            cmd::MADCTL if n >= 1 && !self.applied => {
                self.madctl = self.params[0];
                self.applied = true;
            }
            cmd::COLMOD if n >= 1 && !self.applied => {
                self.colmod = self.params[0];
                self.applied = true;
            }
            cmd::RAMCTRL if n >= 2 && !self.applied => {
                self.ramctrl = [self.params[0], self.params[1]];
                self.applied = true;
            }
            _ if STORED_COMMANDS.contains(&c) => self.store(c),
            _ => {}
        }
    }

    /// The last value wins (the boot table sends 0xD0 twice).
    fn store(&mut self, c: u8) {
        match self.stored.binary_search_by_key(&c, |(key, _)| *key) {
            Ok(at) => self.stored[at].1.clone_from(&self.params),
            Err(at) => self.stored.insert(at, (c, self.params.clone())),
        }
    }

    /// Software reset. MADCTL, COLMOD and frame memory are kept (frame memory UNVERIFIED). The 5 ms
    /// lockout is not modeled: the firmware always waits longer.
    fn swreset(&mut self, t: VTime) {
        self.sleeping = true;
        // SWRESET re-enters sleep-in, so it is a sleep-in edge for the 120 ms rule.
        self.last_slpin = Some(t);
        self.display_on = false;
        self.inverted = false;
        self.xs = 0;
        self.xe = self.col_max();
        self.ys = 0;
        self.ye = self.row_max();
        self.x = 0;
        self.y = 0;
        self.carry = None;
        self.in_pixels = false;
    }

    /// Highest addressable column, which MV exchanges with the row range (default 0..0xEF).
    fn col_max(&self) -> u16 {
        if self.madctl & MADCTL_MV != 0 {
            PANEL_HEIGHT - 1
        } else {
            PANEL_WIDTH - 1
        }
    }

    /// Highest addressable row (default 0..0x13F).
    fn row_max(&self) -> u16 {
        if self.madctl & MADCTL_MV != 0 {
            PANEL_WIDTH - 1
        } else {
            PANEL_HEIGHT - 1
        }
    }
}

fn be16(bytes: &[u8]) -> u16 {
    u16::from(bytes[0]) << 8 | u16::from(bytes[1])
}

impl St7789p3 {
    /// Pixel bytes of a RAMWR or RAMWRC run, `p = b0 << 8 | b1`; an odd segment's last byte waits
    /// in [`St7789p3::carry`]. Only 16 bpp is decoded; any other COLMOD raises
    /// [`TimingLint::RamwrWrongColmod`].
    fn write_pixels(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.carry.take() {
                None => self.carry = Some(byte),
                Some(high) => {
                    let pixel = u16::from(high) << 8 | u16::from(byte);
                    self.store_pixel(pixel);
                }
            }
        }
    }

    /// Stores one pixel and advances. An inverted window addresses nothing, so the pixel is dropped
    /// (UNVERIFIED: the datasheet only requires XS <= XE).
    fn store_pixel(&mut self, pixel: u16) {
        if self.xs > self.xe || self.ys > self.ye {
            return;
        }
        if let Some(index) = self.map(self.x, self.y) {
            self.ram[index] = pixel;
            let row = (index / PANEL_WIDTH as usize) as u16;
            self.mark_row(row);
        }
        self.pixels_written += 1;
        self.advance();
    }

    /// Advances the pointer inside the window: x from xs to xe, then xs with y + 1, wrapping to ys
    /// after ye (the wrap past ye is UNVERIFIED).
    fn advance(&mut self) {
        if self.x >= self.xe {
            self.x = self.xs;
            if self.y >= self.ye {
                self.y = self.ys;
            } else {
                self.y += 1;
            }
        } else {
            self.x += 1;
        }
    }

    /// Maps a window coordinate onto panel memory: MV exchanges the axes, then MX and MY mirror.
    /// `None` (possible under MV) drops the pixel. UNVERIFIED order; this firmware leaves MADCTL at
    /// 0x00.
    fn map(&self, x: u16, y: u16) -> Option<usize> {
        let (mut px, mut py) = if self.madctl & MADCTL_MV != 0 {
            (y, x)
        } else {
            (x, y)
        };
        if px >= PANEL_WIDTH || py >= PANEL_HEIGHT {
            return None;
        }
        if self.madctl & MADCTL_MX != 0 {
            px = PANEL_WIDTH - 1 - px;
        }
        if self.madctl & MADCTL_MY != 0 {
            py = PANEL_HEIGHT - 1 - py;
        }
        Some(py as usize * PANEL_WIDTH as usize + px as usize)
    }

    fn mark_row(&mut self, row: u16) {
        self.dirty = Some(match self.dirty {
            None => (row, row),
            Some((first, last)) => (first.min(row), last.max(row)),
        });
    }

    fn trace_begin(&mut self, c: u8, at: VTime) {
        if self.trace.len() < TRACE_LIMIT {
            self.trace.push(TraceEntry {
                cmd: c,
                params: Vec::new(),
                pixel_bytes: 0,
                at,
            });
            self.trace_cur = Some(self.trace.len() - 1);
        } else {
            self.trace_cur = None;
        }
    }

    /// The timing lint: three class B warnings, never enforced.
    fn lint(&mut self, t: VTime, c: u8) {
        if let Some(at) = self.last_slpout
            && c != cmd::SLPOUT
            && t.0.saturating_sub(at.0) < SLPOUT_SETTLE.0
        {
            self.warn(TimingLint::CommandAfterSlpout, c, t);
        }
        let sleep_edge = match c {
            cmd::SLPIN => self.last_slpout,
            cmd::SLPOUT => self.last_slpin,
            _ => None,
        };
        if let Some(at) = sleep_edge
            && t.0.saturating_sub(at.0) < SLEEP_CYCLE.0
        {
            self.warn(TimingLint::SleepCycleTooFast, c, t);
        }
        if (c == cmd::RAMWR || c == cmd::RAMWRC) && self.colmod != COLMOD_16BPP {
            self.warn(TimingLint::RamwrWrongColmod, c, t);
        }
    }

    /// Records one warning; the list is capped at [`TRACE_LIMIT`], the count is not.
    fn warn(&mut self, lint: TimingLint, c: u8, at: VTime) {
        if self.warnings.len() < TRACE_LIMIT {
            self.warnings.push(TimingWarning { lint, cmd: c, at });
        }
    }
}

impl St7789p3 {
    /// Panel memory, row-major, exactly as the guest wrote it ([`FrameView::Raw`]).
    pub fn raw(&self) -> &[u16] {
        &self.ram
    }

    pub fn pixel(&self, view: FrameView, x: u16, y: u16) -> u16 {
        if x >= PANEL_WIDTH || y >= PANEL_HEIGHT {
            return 0;
        }
        let raw = self.ram[y as usize * PANEL_WIDTH as usize + x as usize];
        self.view_pixel(view, raw)
    }

    /// A whole view as RGB565, row-major, heap-allocated (150 KiB).
    pub fn frame(&self, view: FrameView) -> Vec<u16> {
        match view {
            FrameView::Raw => self.ram.to_vec(),
            _ => self
                .ram
                .iter()
                .map(|&raw| self.view_pixel(view, raw))
                .collect(),
        }
    }

    /// True when the glass emits anything: rail up, out of sleep, and DISPON in force.
    pub fn lit(&self) -> bool {
        self.powered && !self.sleeping && self.display_on
    }

    fn view_pixel(&self, view: FrameView, raw: u16) -> u16 {
        match view {
            FrameView::Raw => raw,
            FrameView::Glass => self.glass_pixel(raw),
            FrameView::Perceived => dim(self.glass_pixel(raw), self.brightness),
        }
    }

    /// [`FrameView::Glass`]: black unless lit, otherwise memory with the `invon_shows_ram` rule
    /// (default: INVON shows memory, INVOFF its complement).
    fn glass_pixel(&self, raw: u16) -> u16 {
        if !self.lit() {
            return 0;
        }
        if self.inverted == self.cfg.invon_shows_ram {
            raw
        } else {
            !raw
        }
    }

    /// Sets the brightness `perceived` and `FramePort` use, from the resolved
    /// [`Backlight::brightness`], not bare `DUTY_R`.
    pub fn set_backlight(&mut self, backlight: &Backlight) {
        self.brightness = backlight.brightness();
        self.frame_duty = self.brightness.frame_duty();
    }

    pub fn brightness(&self) -> Brightness {
        self.brightness
    }

    /// Copies the changed rows into `frame` and publishes the panel flags; returns the rows copied.
    /// The port carries the raw view plus flags, so the glass rule is not baked in. The dirty span
    /// is cleared here, so there must be one publisher. The caller owns `FramePort::present`.
    pub fn publish(&mut self, frame: &mut FramePort) -> Option<(u16, u16)> {
        frame.set_powered(self.powered);
        frame.set_sleeping(self.sleeping);
        frame.set_inverted(self.inverted);
        frame.set_display_on(self.display_on);
        frame.set_invon_shows_ram(self.cfg.invon_shows_ram);
        frame.set_backlight(self.frame_duty);
        let span = self.dirty.take();
        if let Some((first, last)) = span {
            let width = PANEL_WIDTH as usize;
            let pixels = frame.pixels_mut();
            for row in first..=last {
                let at = row as usize * width;
                pixels[at..at + width].copy_from_slice(&self.ram[at..at + width]);
            }
            frame.mark_dirty(first, last);
        }
        span
    }

    /// Marks every row dirty so the next [`St7789p3::publish`] repaints everything. No snapshot
    /// section carries `FramePort`, so a restore calls this before its first publish.
    pub fn mark_all_dirty(&mut self) {
        self.dirty = Some((0, PANEL_HEIGHT - 1));
    }

    pub fn dirty_rows(&self) -> Option<(u16, u16)> {
        self.dirty
    }

    /// How the command trace differs from `specs/st7789-boot.toml`, or `None` when the first
    /// [`BOOT_SEQUENCE`] commands match exactly. Later commands are not compared.
    pub fn boot_mismatch(&self) -> Option<BootMismatch> {
        for (at, step) in BOOT_SEQUENCE.iter().enumerate() {
            match self.trace.get(at) {
                None => {
                    return Some(BootMismatch {
                        step: step.index,
                        expected: Some(*step),
                        found: None,
                    });
                }
                Some(entry) => {
                    if entry.cmd != step.cmd || entry.params.as_slice() != step.params {
                        return Some(BootMismatch {
                            step: step.index,
                            expected: Some(*step),
                            found: Some(entry.clone()),
                        });
                    }
                }
            }
        }
        None
    }
}

/// One RGB565 pixel dimmed by an integer brightness pair, channel by channel. Truncating is the
/// emulator's own choice (UNVERIFIED against the panel's transfer curve).
fn dim(pixel: u16, brightness: Brightness) -> u16 {
    if brightness.is_off() {
        return 0;
    }
    let r = brightness.scale_value(u32::from(pixel >> 11));
    let g = brightness.scale_value(u32::from((pixel >> 5) & 0x3F));
    let b = brightness.scale_value(u32::from(pixel & 0x1F));
    ((r as u16) << 11) | ((g as u16) << 5) | b as u16
}

impl Default for St7789p3 {
    fn default() -> Self {
        St7789p3::new(PanelConfig::default())
    }
}

impl SpiDevice for St7789p3 {
    fn transfer(&mut self, t: VTime, dc: bool, data: &[u8], cs_release: bool) {
        St7789p3::transfer(self, t, dc, data, cs_release);
    }
}

impl Chip for St7789p3 {
    /// Only `board_rail` resets the panel, clearing memory with it (class C); SWRESET is a command.
    fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::BoardRail {
            let cfg = self.cfg;
            *self = St7789p3::new(cfg);
            // `publish` copies dirty rows only.
            self.mark_all_dirty();
        }
    }
}
