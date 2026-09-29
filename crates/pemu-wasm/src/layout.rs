//! Layout constants of the wasm ABI, the single source `xtask wasm` turns into
//! `web/src/worker/layout.ts`. Every item is a constant, a `repr(C)` type or a total function
//! over one, so the generator and the wasm core agree by construction.
//!
//! The core is single-threaded on the worker's own thread, so no ring changes while JavaScript
//! reads it: the cursor block ([`IoLayout::cursors_ptr`]) is refreshed when a call returns, and
//! plain loads suffice on the JS side.

use core::mem::{align_of, offset_of, size_of};

use pemu_core::hostio::{EventKind, SerialStream};
use pemu_machine::stops::StopReason;

/// Version `pemu_abi_version` returns; the web worker checks it at load. A change to the ABI
/// functions or to any layout in this module bumps it.
pub const ABI_VERSION: u32 = 3;

/// `ResultHeader::status` of a successful call; any other status is an `ErrorCode` number and the
/// payload is an `ApiError` JSON.
pub const STATUS_OK: u32 = 0;

/// Result header (12 bytes) every ABI call returning `res` points to; `pemu_result_free` frees
/// the header and its payload.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResultHeader {
    pub ptr: u32,
    pub len: u32,
    /// `STATUS_OK`, else an `ErrorCode` number.
    pub status: u32,
}

const _: () = assert!(size_of::<ResultHeader>() == 12);
const _: () = assert!(offset_of!(ResultHeader, ptr) == 0);
const _: () = assert!(offset_of!(ResultHeader, len) == 4);
const _: () = assert!(offset_of!(ResultHeader, status) == 8);

/// Asset kind of `pemu_load`, with the `pemu-loader` parser each kind uses.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LoadKind {
    /// 0: ROM ELF, an optional override the web UI never sends;
    /// `pemu_loader::rom::RomImage::from_elf`.
    RomElf = 0,
    /// 1: merged flash image; `pemu_loader::bundle::FlashImage::from_merged`.
    MergedFlash = 1,
    /// 2: app ELF; `pemu_loader::elf::ElfInfo::parse`.
    AppElf = 2,
    /// 3: bootloader ELF; `pemu_loader::elf::ElfInfo::parse`.
    BootloaderElf = 3,
    /// 4: eFuse image; `pemu_loader::efuse_image::EfuseImage::from_dump`.
    Efuse = 4,
}

impl LoadKind {
    pub const ALL: [LoadKind; 5] = [
        LoadKind::RomElf,
        LoadKind::MergedFlash,
        LoadKind::AppElf,
        LoadKind::BootloaderElf,
        LoadKind::Efuse,
    ];

    /// The kind for a `pemu_load` `kind` argument, or `None` for an unknown number.
    pub const fn from_u32(kind: u32) -> Option<LoadKind> {
        match kind {
            0 => Some(LoadKind::RomElf),
            1 => Some(LoadKind::MergedFlash),
            2 => Some(LoadKind::AppElf),
            3 => Some(LoadKind::BootloaderElf),
            4 => Some(LoadKind::Efuse),
            _ => None,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            LoadKind::RomElf => "RomElf",
            LoadKind::MergedFlash => "MergedFlash",
            LoadKind::AppElf => "AppElf",
            LoadKind::BootloaderElf => "BootloaderElf",
            LoadKind::Efuse => "Efuse",
        }
    }
}

/// The `u32` `pemu_run` returns: the discriminant of `pemu_machine::stops::StopReason` without its
/// payload, which `pemu_last_stop` carries as JSON. Numbered in the declaration order of
/// `StopReason`; [`stop_code`] is an exhaustive match, so a new variant cannot renumber the ABI.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum StopCode {
    /// Reached the `until_ps` of `pemu_run`.
    Until = 0,
    /// Reached the `max_insns` of `pemu_run`.
    MaxInsns = 1,
    /// An armed console or event matcher fired.
    Matcher = 2,
    Breakpoint = 3,
    Watchpoint = 4,
    GuestPanic = 5,
    Deadlock = 6,
    /// The hang detector produced a `StuckReport`.
    Stuck = 7,
    Tripwire = 8,
    /// Strict mode: an unmodeled first touch.
    Unmodeled = 9,
    /// An HLE hook failed.
    Hle = 10,
    /// The hart halted; the `HaltCause` travels in `pemu_last_stop`.
    Halted = 11,
    /// Reserved: the machine sequences resets and no longer produces it; a reset stop is a
    /// `Matcher` stop on the reset event.
    ChipReset = 12,
    /// A sleep entry the machine does not perform yet.
    Sleep = 13,
}

impl StopCode {
    pub const ALL: [StopCode; 14] = [
        StopCode::Until,
        StopCode::MaxInsns,
        StopCode::Matcher,
        StopCode::Breakpoint,
        StopCode::Watchpoint,
        StopCode::GuestPanic,
        StopCode::Deadlock,
        StopCode::Stuck,
        StopCode::Tripwire,
        StopCode::Unmodeled,
        StopCode::Hle,
        StopCode::Halted,
        StopCode::ChipReset,
        StopCode::Sleep,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            StopCode::Until => "Until",
            StopCode::MaxInsns => "MaxInsns",
            StopCode::Matcher => "Matcher",
            StopCode::Breakpoint => "Breakpoint",
            StopCode::Watchpoint => "Watchpoint",
            StopCode::GuestPanic => "GuestPanic",
            StopCode::Deadlock => "Deadlock",
            StopCode::Stuck => "Stuck",
            StopCode::Tripwire => "Tripwire",
            StopCode::Unmodeled => "Unmodeled",
            StopCode::Hle => "Hle",
            StopCode::Halted => "Halted",
            StopCode::ChipReset => "ChipReset",
            StopCode::Sleep => "Sleep",
        }
    }

    /// Whether the stop is a limit the pacing loop expects every slice; every other code is an
    /// event the worker reports and that ends the pacing loop.
    pub const fn is_limit(self) -> bool {
        matches!(self, StopCode::Until | StopCode::MaxInsns)
    }
}

/// The discriminant `pemu_run` returns for `reason`. Exhaustive on purpose: the numbering is
/// frozen, so a new `StopReason` variant needs a number here and an [`ABI_VERSION`] bump.
pub const fn stop_code(reason: &StopReason) -> StopCode {
    match reason {
        StopReason::Until => StopCode::Until,
        StopReason::MaxInsns => StopCode::MaxInsns,
        StopReason::Matcher(_) => StopCode::Matcher,
        StopReason::Breakpoint(_) => StopCode::Breakpoint,
        StopReason::Watchpoint { .. } => StopCode::Watchpoint,
        StopReason::GuestPanic(_) => StopCode::GuestPanic,
        StopReason::Deadlock => StopCode::Deadlock,
        StopReason::Stuck(_) => StopCode::Stuck,
        StopReason::Tripwire(_) => StopCode::Tripwire,
        StopReason::Unmodeled(_) => StopCode::Unmodeled,
        StopReason::Hle(_) => StopCode::Hle,
        StopReason::Halted(_) => StopCode::Halted,
        StopReason::ChipReset(_) => StopCode::ChipReset,
        StopReason::Sleep(_) => StopCode::Sleep,
    }
}

/// Columns of the ST7789P3 panel framebuffer (240x320 RGB565). Derived from `pemu_core::hostio`,
/// never restated, so `layout.ts` and the runtime `FramePort::width` cannot disagree.
pub const FRAME_WIDTH: u32 = pemu_core::hostio::FRAME_WIDTH as u32;
/// Rows of the panel framebuffer, derived as [`FRAME_WIDTH`] is.
pub const FRAME_HEIGHT: u32 = pemu_core::hostio::FRAME_HEIGHT as u32;
/// `IoLayout::frame_dirty_first` when no row changed since the worker last uploaded.
pub const DIRTY_NONE: u32 = u32::MAX;

/// Backlight denominator published until the LEDC duty resolution is modelled; brightness is
/// `(duty >> 4, 1 << duty_res)` and `FramePort` carries only the numerator.
///
/// UNVERIFIED: 10-bit duty is the common LEDC configuration, not a measured one.
pub const DEFAULT_BACKLIGHT_SCALE: u32 = 1 << 10;

/// Bit of `IoLayout::frame_flags`: the panel rail is on.
pub const FRAME_FLAG_POWERED: u32 = 1 << 0;
/// Bit of `IoLayout::frame_flags`: the panel is in sleep-in.
pub const FRAME_FLAG_SLEEPING: u32 = 1 << 1;
/// Bit of `IoLayout::frame_flags`: INVON is on. The command state, not what the glass shows; a
/// renderer uses [`FRAME_FLAG_GLASS_COMPLEMENT`].
pub const FRAME_FLAG_INVERTED: u32 = 1 << 2;
/// Bit of `IoLayout::frame_flags`: DISPON is in force; without it the glass is black and panel
/// memory is kept.
pub const FRAME_FLAG_DISPLAY_ON: u32 = 1 << 3;
/// Bit of `IoLayout::frame_flags`: the glass shows the complement of panel memory, i.e. INVON
/// disagrees with the board's `invon_shows_ram`.
pub const FRAME_FLAG_GLASS_COMPLEMENT: u32 = 1 << 4;

/// A ring the worker maps as a typed array, in the ABI order of [`IoLayout::rings`]. `net` and
/// `hci` are not published as rings.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum RingId {
    /// `HostIo::usj_tx` bytes, guest to host.
    UsjTx = 0,
    /// `HostIo::usj_rx` bytes, host to guest.
    UsjRx = 1,
    /// `HostIo::uart0_tx` bytes, guest to host.
    Uart0Tx = 2,
    /// `HostIo::audio_out` samples, `i16`.
    AudioOutSamples = 3,
    /// `HostIo::audio_out` record headers, [`PcmRecordAbi`].
    AudioOutRecords = 4,
    /// `HostIo::audio_in` samples, `i16`.
    AudioInSamples = 5,
    /// `HostIo::audio_in` record headers, [`PcmRecordAbi`].
    AudioInRecords = 6,
    /// `HostIo::events`, [`HostEventAbi`].
    Events = 7,
    /// `HostIo::lines` marks of `usj_tx`, [`LineMarkAbi`].
    LinesUsjTx = 8,
    /// `HostIo::lines` marks of `uart0_tx`, [`LineMarkAbi`].
    LinesUart0Tx = 9,
}

pub const RING_COUNT: usize = 10;

impl RingId {
    pub const ALL: [RingId; RING_COUNT] = [
        RingId::UsjTx,
        RingId::UsjRx,
        RingId::Uart0Tx,
        RingId::AudioOutSamples,
        RingId::AudioOutRecords,
        RingId::AudioInSamples,
        RingId::AudioInRecords,
        RingId::Events,
        RingId::LinesUsjTx,
        RingId::LinesUart0Tx,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            RingId::UsjTx => "UsjTx",
            RingId::UsjRx => "UsjRx",
            RingId::Uart0Tx => "Uart0Tx",
            RingId::AudioOutSamples => "AudioOutSamples",
            RingId::AudioOutRecords => "AudioOutRecords",
            RingId::AudioInSamples => "AudioInSamples",
            RingId::AudioInRecords => "AudioInRecords",
            RingId::Events => "Events",
            RingId::LinesUsjTx => "LinesUsjTx",
            RingId::LinesUart0Tx => "LinesUart0Tx",
        }
    }

    /// The serial stream a byte ring or line-mark ring belongs to, for [`LineIndex`] reads.
    ///
    /// [`LineIndex`]: pemu_core::hostio::LineIndex
    pub const fn stream(self) -> Option<SerialStream> {
        match self {
            RingId::UsjTx | RingId::LinesUsjTx => Some(SerialStream::UsjTx),
            RingId::Uart0Tx | RingId::LinesUart0Tx => Some(SerialStream::Uart0Tx),
            _ => None,
        }
    }

    /// Index of the ring's `head` cell in the cursor block; `tail` is the next cell.
    pub const fn head_slot(self) -> u32 {
        (self as u32) * 2
    }

    pub const fn tail_slot(self) -> u32 {
        (self as u32) * 2 + 1
    }
}

/// One ring in [`IoLayout::rings`]. `buf_ptr` addresses `capacity * elem_size` bytes; slot `pos`
/// is at `(pos % capacity) * elem_size`. `head` and `tail` live in the cursor block, so a view of
/// the buffer survives every push.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct RingLayout {
    pub buf_ptr: u32,
    /// Slots in the buffer; the window `[tail, head)` is never longer.
    pub capacity: u32,
    pub elem_size: u32,
    /// Index of the ring's `head` cell in the cursor block ([`RingId::head_slot`]).
    pub cursor_slot: u32,
}

const _: () = assert!(size_of::<RingLayout>() == 16);
const _: () = assert!(offset_of!(RingLayout, buf_ptr) == 0);
const _: () = assert!(offset_of!(RingLayout, capacity) == 4);
const _: () = assert!(offset_of!(RingLayout, elem_size) == 8);
const _: () = assert!(offset_of!(RingLayout, cursor_slot) == 12);

/// Cursor cell holding the `FramePort` generation.
pub const SLOT_FRAME_GENERATION: u32 = (RING_COUNT as u32) * 2;
/// Cursor cell counting `audio_out` underflows.
pub const SLOT_AUDIO_OUT_UNDERFLOWS: u32 = SLOT_FRAME_GENERATION + 1;
/// Cursor cell counting `audio_in` underflows (an underflow yields zeros).
pub const SLOT_AUDIO_IN_UNDERFLOWS: u32 = SLOT_FRAME_GENERATION + 2;
/// Cursor cell holding virtual time in picoseconds, as `pemu_now_ps` would return it.
pub const SLOT_NOW_PS: u32 = SLOT_FRAME_GENERATION + 3;
/// Cells in the cursor block, each a little-endian `u64` (`SLOT_NOW_PS` is read as `i64`).
pub const CURSOR_SLOTS: u32 = SLOT_FRAME_GENERATION + 4;

/// What `pemu_io_layout` points at: offsets of every ring and the frame buffer, plus a layout
/// generation counter.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct IoLayout {
    /// [`ABI_VERSION`], repeated here so a stale view is caught without a second call.
    pub abi_version: u32,
    /// Bumped by anything that changes a buffer address (a restore or rebuild); the worker then
    /// re-creates its views.
    pub generation: u32,
    /// Address of `CURSOR_SLOTS` little-endian `u64` cells, 8-byte aligned.
    pub cursors_ptr: u32,
    /// [`CURSOR_SLOTS`], repeated so a view can be built from this struct alone.
    pub cursor_slots: u32,
    /// Address of `FRAME_WIDTH * FRAME_HEIGHT` RGB565 pixels, row-major.
    pub frame_ptr: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub frame_flags: u32,
    /// Backlight duty numerator, brightness being `(duty >> 4, 1 << duty_res)`: `IoPublisher`
    /// applies the `>> 4` to the raw LEDC duty, so this and [`IoLayout::frame_backlight_scale`]
    /// are on one scale.
    pub frame_backlight: u32,
    /// Backlight denominator, `1 << duty_res`. 0 means unknown, and the panel reads fully lit.
    pub frame_backlight_scale: u32,
    /// First row changed since the *previous* refresh, or [`DIRTY_NONE`]. A delta, not a running
    /// union: a host that skips an upload has to union the skipped spans itself.
    pub frame_dirty_first: u32,
    /// Last dirty row (inclusive), meaningless when `frame_dirty_first` is [`DIRTY_NONE`].
    pub frame_dirty_last: u32,
    pub rings: [RingLayout; RING_COUNT],
}

const _: () = assert!(align_of::<IoLayout>() == 4);
const _: () = assert!(offset_of!(IoLayout, rings) == 48);
const _: () = assert!(size_of::<IoLayout>() == 48 + 16 * RING_COUNT);

impl Default for IoLayout {
    fn default() -> Self {
        IoLayout {
            abi_version: ABI_VERSION,
            generation: 0,
            cursors_ptr: 0,
            cursor_slots: CURSOR_SLOTS,
            frame_ptr: 0,
            frame_width: FRAME_WIDTH,
            frame_height: FRAME_HEIGHT,
            frame_flags: 0,
            frame_backlight: 0,
            frame_backlight_scale: DEFAULT_BACKLIGHT_SCALE,
            frame_dirty_first: DIRTY_NONE,
            frame_dirty_last: 0,
            rings: [RingLayout {
                buf_ptr: 0,
                capacity: 0,
                elem_size: 0,
                cursor_slot: 0,
            }; RING_COUNT],
        }
    }
}

/// A `PcmRecord` as the worker reads it. The Rust struct's field order is not fixed, so the ABI
/// publishes this `repr(C)` mirror; the sample slices are published in place.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct PcmRecordAbi {
    /// Virtual time of the first frame of the run, in picoseconds.
    pub vt_start_ps: i64,
    /// Absolute sample cursor of the first sample of the run.
    pub first: u64,
    /// Sample rate in Hz: the guest rate is dynamic (16 kHz and 24 kHz both occur), so the
    /// worklet reads it per record.
    pub fs: u32,
    /// Interleaved channels per frame.
    pub channels: u32,
}

const _: () = assert!(size_of::<PcmRecordAbi>() == 24);
const _: () = assert!(offset_of!(PcmRecordAbi, vt_start_ps) == 0);
const _: () = assert!(offset_of!(PcmRecordAbi, first) == 8);
const _: () = assert!(offset_of!(PcmRecordAbi, fs) == 16);
const _: () = assert!(offset_of!(PcmRecordAbi, channels) == 20);

/// The `repr(C)` mirror of `pemu_core::hostio::HostEvent`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct HostEventAbi {
    /// Virtual time it happened, in picoseconds.
    pub vt_ps: i64,
    /// Kind-specific argument.
    pub arg: u64,
    /// An [`EventKind`] number ([`event_kind_code`]).
    pub kind: u32,
    /// Zero; keeps the record 8-byte aligned on every target.
    pub reserved: u32,
}

const _: () = assert!(size_of::<HostEventAbi>() == 24);
const _: () = assert!(offset_of!(HostEventAbi, kind) == 16);

/// The `repr(C)` mirror of `pemu_core::hostio::LineMark`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LineMarkAbi {
    /// Absolute cursor of the `\n` byte in the stream's byte ring.
    pub offset: u64,
    /// Virtual time the byte was emitted, in picoseconds.
    pub vt_ps: i64,
    /// A [`SerialStream`] number ([`serial_stream_code`]).
    pub stream: u32,
    /// Zero; keeps the record 8-byte aligned on every target.
    pub reserved: u32,
}

const _: () = assert!(size_of::<LineMarkAbi>() == 24);
const _: () = assert!(offset_of!(LineMarkAbi, stream) == 16);

pub const EVENT_KINDS: [EventKind; 7] = [
    EventKind::Reset,
    EventKind::Panic,
    EventKind::Sleep,
    EventKind::Power,
    EventKind::Frame,
    EventKind::UiSettled,
    EventKind::FidelityWarning,
];

/// The `HostEventAbi::kind` number of `kind`. Exhaustive, so a kind added to `pemu-core` must be
/// numbered here.
pub const fn event_kind_code(kind: EventKind) -> u32 {
    match kind {
        EventKind::Reset => 0,
        EventKind::Panic => 1,
        EventKind::Sleep => 2,
        EventKind::Power => 3,
        EventKind::Frame => 4,
        EventKind::UiSettled => 5,
        EventKind::FidelityWarning => 6,
    }
}

pub const fn event_kind_name(kind: EventKind) -> &'static str {
    match kind {
        EventKind::Reset => "Reset",
        EventKind::Panic => "Panic",
        EventKind::Sleep => "Sleep",
        EventKind::Power => "Power",
        EventKind::Frame => "Frame",
        EventKind::UiSettled => "UiSettled",
        EventKind::FidelityWarning => "FidelityWarning",
    }
}

/// The `LineMarkAbi::stream` number of `stream`; `SerialStream::index` is the same dense order.
pub const fn serial_stream_code(stream: SerialStream) -> u32 {
    stream.index() as u32
}

pub const fn serial_stream_name(stream: SerialStream) -> &'static str {
    match stream {
        SerialStream::UsjTx => "UsjTx",
        SerialStream::Uart0Tx => "Uart0Tx",
    }
}

/// First word of a fixed-layout `pemu_input` batch, `PEIN` little-endian. No magic byte is `[` or
/// `{`, so the first byte tells it from JSON, and the magic rejects a truncated or foreign buffer.
pub const INPUT_BATCH_MAGIC: u32 = u32::from_le_bytes(*b"PEIN");

/// Header of a fixed-layout `pemu_input` batch: `count` [`InputRecordAbi`] follow it, then the
/// blob area that variable-length events point into.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InputBatchHeader {
    pub magic: u32,
    pub count: u32,
    pub blob_off: u32,
    pub blob_len: u32,
}

const _: () = assert!(size_of::<InputBatchHeader>() == 16);

/// One journaled input in a fixed-layout batch. The three scalar words carry the small fields of
/// each kind; variable-length payloads live in the blob area at `blob_off .. blob_off + blob_len`
/// from the start of the buffer.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct InputRecordAbi {
    /// Virtual time the input applies at, in picoseconds, or [`AT_NOW`]. The worker stamps
    /// interactive input with the next slice boundary, so a paced session replays as itself.
    pub at_ps: i64,
    pub kind: u32,
    pub a: u32,
    /// Second scalar word.
    pub b: u32,
    /// Third scalar word.
    pub c: u32,
    /// Offset of this record's payload from the start of the buffer, 0 when there is none.
    pub blob_off: u32,
    /// Length of this record's payload in bytes.
    pub blob_len: u32,
}

const _: () = assert!(size_of::<InputRecordAbi>() == 32);
const _: () = assert!(offset_of!(InputRecordAbi, at_ps) == 0);
const _: () = assert!(offset_of!(InputRecordAbi, kind) == 8);
const _: () = assert!(offset_of!(InputRecordAbi, blob_off) == 24);

/// [`InputRecordAbi::at_ps`] meaning `At::Now`: the core stamps the input itself. No real instant
/// is negative.
pub const AT_NOW: i64 = -1;

/// Kind of an [`InputRecordAbi`], in the declaration order of `pemu_core::input::InputEvent`.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// `a` = button (0 Up, 1 Down, 2 Ok), `b` = 1 when pressed.
    Button = 0,
    /// `b` = 1 when pressed.
    Power = 1,
    /// `b` = 1 when plugged.
    UsbCable = 2,
    /// `b` = 1 when a client holds the port open.
    UsbClient = 3,
    /// `a` = DTR, `b` = RTS.
    UsbLine = 4,
    /// `a` = channel; payload = the bytes.
    SerialIn = 5,
    /// `a` = per-mille state of charge, `b` = millivolts, `c` = deci-degrees plus a present bit;
    /// provisional until `BatterySet` fixes the split.
    Battery = 6,
    /// Payload = the NFC operations (`NfcOp`).
    NfcTap = 7,
    /// `a` = sequence number low word, `b` = high word; payload = `i16` samples.
    MicChunk = 8,
    /// `a` = sequence number low word, `b` = high word; payload = the frame.
    NetFrame = 9,
    /// `a` = sequence number low word, `b` = high word; payload = the packet.
    HciPacket = 10,
    /// `a` = unix microseconds low word, `b` = high word.
    RtcEpoch = 11,
    /// Payload = the environment change (`EnvChange`).
    Env = 12,
}

impl InputKind {
    pub const ALL: [InputKind; 13] = [
        InputKind::Button,
        InputKind::Power,
        InputKind::UsbCable,
        InputKind::UsbClient,
        InputKind::UsbLine,
        InputKind::SerialIn,
        InputKind::Battery,
        InputKind::NfcTap,
        InputKind::MicChunk,
        InputKind::NetFrame,
        InputKind::HciPacket,
        InputKind::RtcEpoch,
        InputKind::Env,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            InputKind::Button => "Button",
            InputKind::Power => "Power",
            InputKind::UsbCable => "UsbCable",
            InputKind::UsbClient => "UsbClient",
            InputKind::UsbLine => "UsbLine",
            InputKind::SerialIn => "SerialIn",
            InputKind::Battery => "Battery",
            InputKind::NfcTap => "NfcTap",
            InputKind::MicChunk => "MicChunk",
            InputKind::NetFrame => "NetFrame",
            InputKind::HciPacket => "HciPacket",
            InputKind::RtcEpoch => "RtcEpoch",
            InputKind::Env => "Env",
        }
    }
}

/// Button number of [`InputKind::Button`], in the declaration order of `ButtonId`.
pub const BUTTON_IDS: [(&str, u32); 3] = [("Up", 0), ("Down", 1), ("Ok", 2)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_header_is_12_bytes() {
        assert_eq!(size_of::<ResultHeader>(), 12);
        assert_eq!(align_of::<ResultHeader>(), 4);
    }

    #[test]
    fn load_kind_numbers_match_arch() {
        for n in 0..=4u32 {
            let kind = LoadKind::from_u32(n).expect("kinds 0 to 4 exist");
            assert_eq!(kind as u32, n);
        }
        assert_eq!(LoadKind::from_u32(5), None);
        assert_eq!(LoadKind::from_u32(u32::MAX), None);
        for (n, kind) in LoadKind::ALL.iter().enumerate() {
            assert_eq!(*kind as u32, n as u32);
        }
    }

    #[test]
    fn stop_codes_are_dense_and_in_declaration_order() {
        for (n, code) in StopCode::ALL.iter().enumerate() {
            assert_eq!(*code as u32, n as u32);
        }
        assert_eq!(stop_code(&StopReason::Until), StopCode::Until);
        assert_eq!(stop_code(&StopReason::MaxInsns), StopCode::MaxInsns);
        assert_eq!(stop_code(&StopReason::Deadlock), StopCode::Deadlock);
        assert_eq!(
            stop_code(&StopReason::Breakpoint(0x4000_0000)),
            StopCode::Breakpoint
        );
        assert!(StopCode::Until.is_limit() && StopCode::MaxInsns.is_limit());
        assert!(!StopCode::GuestPanic.is_limit());
    }

    #[test]
    fn every_ring_owns_two_cursor_cells_below_the_extra_slots() {
        for (n, ring) in RingId::ALL.iter().enumerate() {
            assert_eq!(*ring as u32, n as u32);
            assert_eq!(ring.head_slot(), 2 * n as u32);
            assert_eq!(ring.tail_slot(), 2 * n as u32 + 1);
            assert!(ring.tail_slot() < SLOT_FRAME_GENERATION);
        }
        assert_eq!(CURSOR_SLOTS, 2 * RING_COUNT as u32 + 4);
        assert_eq!(SLOT_NOW_PS, CURSOR_SLOTS - 1);
    }

    #[test]
    fn line_rings_and_byte_rings_agree_on_their_stream() {
        assert_eq!(RingId::UsjTx.stream(), Some(SerialStream::UsjTx));
        assert_eq!(RingId::LinesUsjTx.stream(), Some(SerialStream::UsjTx));
        assert_eq!(RingId::Uart0Tx.stream(), Some(SerialStream::Uart0Tx));
        assert_eq!(RingId::LinesUart0Tx.stream(), Some(SerialStream::Uart0Tx));
        assert_eq!(RingId::Events.stream(), None);
        assert_eq!(RingId::AudioOutSamples.stream(), None);
    }

    #[test]
    fn the_default_layout_announces_the_abi_version_and_frame_size() {
        let layout = IoLayout::default();
        assert_eq!(layout.abi_version, ABI_VERSION);
        assert_eq!(layout.cursor_slots, CURSOR_SLOTS);
        assert_eq!(layout.frame_width, FRAME_WIDTH);
        assert_eq!(layout.frame_height, FRAME_HEIGHT);
        assert_eq!(layout.frame_dirty_first, DIRTY_NONE);
        assert_eq!(layout.rings.len(), RING_COUNT);
    }

    #[test]
    fn input_kinds_are_dense_and_the_magic_is_not_json() {
        for (n, kind) in InputKind::ALL.iter().enumerate() {
            assert_eq!(*kind as u32, n as u32);
        }
        let magic = INPUT_BATCH_MAGIC.to_le_bytes();
        assert_eq!(&magic, b"PEIN");
        assert_ne!(magic[0], b'[');
        assert_ne!(magic[0], b'{');
        const {
            assert!(
                AT_NOW < 0,
                "virtual time is never negative, so AT_NOW cannot collide"
            )
        };
    }

    #[test]
    fn event_kind_codes_are_dense_and_named() {
        for (n, kind) in EVENT_KINDS.iter().enumerate() {
            assert_eq!(event_kind_code(*kind), n as u32);
            assert!(!event_kind_name(*kind).is_empty());
        }
        assert_eq!(event_kind_code(EventKind::default()), 0);
    }

    #[test]
    fn serial_stream_codes_follow_the_core_index() {
        for stream in SerialStream::ALL {
            assert_eq!(serial_stream_code(stream), stream.index() as u32);
            assert!(!serial_stream_name(stream).is_empty());
        }
    }
}
