//! USB Serial/JTAG: the CDC-ACM device the Passport console runs over (`specs/blocks/usj.toml`).
//!
//! Two 64-byte endpoints plus an interrupt register file. Everything it does depends on state
//! outside the chip (cable, rail, whether a client has the port open), kept in [`HostLink`] and
//! pushed in by `wiring/usj.rs` from `HostIo::usj_ctrl`.
//!
//! SOF is load-bearing: IDF's connection monitor, a tick hook, samples and clears the `SOF` raw
//! bit and declares the port disconnected after three ticks without one; then
//! `usb_serial_jtag_write` returns -1 and every console byte is dropped. So [`UsjModel`] raises one
//! SOF per emulated millisecond exactly while the link is enumerated.
//!
//! | U-state | SOF | committed IN packet | OUT data |
//! |---|---|---|---|
//! | U0 DETACHED, U1 CHARGE_ONLY | no | never taken | none |
//! | U2 ATTACHED_IDLE | yes | never taken (UNVERIFIED) | none |
//! | U3 ATTACHED_OPEN | yes | taken at the next host poll | packets of at most 64 bytes |
//!
//! Under `fast` a committed IN packet is taken at once, under `device` after one host poll
//! ([`UsjModel::drain_interval`]): the ROM and bootloader wait in real time for the console to
//! drain, so an instant drain makes every later timestamp early. The ROM's `usb_uart_tx_one_char`
//! gives up after 5000 polls and drops the character, so the latency is bounded above.
//!
//! The OUT direction is paced by the SOF tick ([`UsjModel::tick`]): a guest that only reads never
//! returns a `Wiring`, and reading is all the ROM download path does while it receives a body.

use pemu_core::fidelity::{Fidelity, FidelityLedger, FirstTouch, TouchAccess};
use pemu_core::hostio::{UsbHostState, UsjEnumeration};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::{ResetCause, ResetKind, ResetScope};
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Deserializer, Serialize, Serializer};
use pemu_core::time::VTime;

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block, id};
use crate::r#gen::regs_usj::{BLOCK_SIZE, REG_COUNT, REGS, idx};

const _: () = assert!(BLOCK_SIZE == <block::Usj as Block>::SIZE);

/// Bytes of one endpoint FIFO: the CDC-ACM bulk packet size of the C3.
pub const FIFO_BYTES: usize = 64;

/// Virtual time between two SOF packets: 1 kHz on a full-speed bus.
pub const SOF_PERIOD: VTime = VTime(1_000_000_000);

const TAG_SOF: u16 = 0;

const TAG_ENUM: u16 = 1;

/// Event tag of the IN drain: the host poll that takes the committed packet.
const TAG_DRAIN: u16 = 2;

/// Interrupt source of the block: 26, `ETS_USB_SERIAL_JTAG_INTR_SOURCE`.
pub const SOURCE: IrqSource = irq::USB_SERIAL_JTAG;

pub const INT_SOF: u32 = 1 << 1;
pub const INT_OUT_RECV_PKT: u32 = 1 << 2;
pub const INT_IN_EMPTY: u32 = 1 << 3;
pub const INT_IN_TOKEN_REC_EP1: u32 = 1 << 8;
pub const INT_BUS_RESET: u32 = 1 << 9;

/// Bits of INT_RAW this model ever drives; the error and JTAG bits stay 0.
const INT_MODELED: u32 =
    INT_SOF | INT_OUT_RECV_PKT | INT_IN_EMPTY | INT_IN_TOKEN_REC_EP1 | INT_BUS_RESET;

const EP1_CONF_WR_DONE: u32 = 1 << 0;
const EP1_CONF_IN_DATA_FREE: u32 = 1 << 1;
const EP1_CONF_OUT_DATA_AVAIL: u32 = 1 << 2;

const CONF0_PAD_PULL_OVERRIDE: u32 = 1 << 8;
const CONF0_DP_PULLUP: u32 = 1 << 9;
const CONF0_PAD_ENABLE: u32 = 1 << 14;

/// `FRAM_NUM.SOF_FRAME_INDEX` is 11 bits in the generated table (the imported IDF header), so the
/// frame counter wraps at 2048. UNVERIFIED width; nothing the firmware does depends on the top bit.
const FRAME_MASK: u16 = 0x7FF;

/// Bytes of console text the model buffers before `wiring/usj.rs` moves them into
/// `HostIo::usj_tx`. The pump runs on every `Wiring::UsjIo`, so one packet is the normal
/// occupancy; the cap only bounds a caller that ignores the wiring effect.
const CAPTURE_CAP: usize = 4 * FIFO_BYTES;

/// Console capture mode. `pemu-machine` maps its own `CaptureMode` onto this one.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub enum Capture {
    /// Record what a host actually receives: bytes reach the capture when the host takes the
    /// packet, so a detached run records nothing.
    #[default]
    Wire,
    /// Also record bytes the guest committed while detached (class U observability).
    Fifo,
}

/// The host side of the link: which U-state the cable, the MCU rail and the client put the
/// device in, and how far enumeration has got. The model never reads `HostIo` itself.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct HostLink {
    pub state: UsbHostState,
    pub enumeration: UsjEnumeration,
}

impl HostLink {
    /// The default host state: cable plugged, rail on, a client with the port open, enumerated.
    /// Every new machine starts here.
    pub const OPEN: HostLink = HostLink {
        state: UsbHostState::AttachedOpen,
        enumeration: UsjEnumeration::Enumerated,
    };

    pub const DETACHED: HostLink = HostLink {
        state: UsbHostState::Detached,
        enumeration: UsjEnumeration::Detached,
    };

    pub const IDLE: HostLink = HostLink {
        state: UsbHostState::AttachedIdle,
        enumeration: UsjEnumeration::Enumerated,
    };

    pub fn enumerated(self) -> bool {
        self.enumeration == UsjEnumeration::Enumerated
            && matches!(
                self.state,
                UsbHostState::AttachedIdle | UsbHostState::AttachedOpen
            )
    }

    pub fn open(self) -> bool {
        self.enumerated() && self.state == UsbHostState::AttachedOpen
    }
}

/// What a `SET_CONTROL_LINE_STATE` did, for the caller that owns the chip.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum LineAction {
    /// (RTS, DTR) = (1, 1), or a repeat of a line state that already reset the chip: nothing.
    None,
    /// (0, 0) cleared or (0, 1) set the download flag; the new value is carried.
    DownloadFlag(bool),
    /// Entry into (RTS, DTR) = (1, 0): a system reset with cause 0x15 `USB_UART_CHIP_RESET`.
    ChipReset {
        kind: ResetKind,
        /// Value `GPIO_STRAP` latches: the download flag as it stood when the reset fired.
        download: bool,
    },
}

pub type Model = UsjModel;

/// The USB Serial/JTAG model: two 64-byte endpoints, the interrupt file, the SOF generator and
/// the line-state machine.
pub struct UsjModel {
    regs: RegStore<REG_COUNT>,
    link: HostLink,
    capture_mode: Capture,
    in_stage: Vec<u8>,
    /// The committed IN packet waiting for the host; in U0 and U2 it never is taken.
    in_packet: Option<Vec<u8>>,
    out: Vec<u8>,
    out_pos: usize,
    capture: Vec<u8>,
    /// Bytes the capture buffer had no room for; reported, never dropped silently.
    capture_lost: u64,
    /// The download flag of the line-state table. It is chip state outside the register file and
    /// survives the reset it causes, which is how esptool's sequences reach download mode.
    download: bool,
    /// The last `(rts, dtr)` applied, so a repeated line state is not a second reset (esptool
    /// writes DTR twice for `usbser.sys`).
    line: (bool, bool),
    sof: Option<EventHandle>,
    pending: Option<EventHandle>,
    drain: Option<EventHandle>,
    drain_ps: VTime,
    /// Enumeration delay after the link returns (a plug, a deep-sleep wake): the profile's
    /// `usj_enum_wake_ps`, 0 under `fast`.
    enum_delay: VTime,
    /// Enumeration delay after a `SYS_` class reset, which drops the link although the host state
    /// does not change: the profile's `usj_enum_reset_ps`.
    reset_enum_delay: VTime,
    frame: u16,
    /// The level last driven onto source 26, so the fabric is touched only on a change.
    irq_driven: bool,
    /// Bit per register already reported to the ledger; snapshotted.
    touched: u32,
}

fn key(tag: u16) -> EventKey {
    EventKey {
        owner: Owner::Periph(id::USJ),
        tag,
    }
}

impl Default for UsjModel {
    fn default() -> Self {
        let mut m = UsjModel {
            regs: RegStore::new(&REGS),
            link: HostLink::OPEN,
            capture_mode: Capture::Wire,
            in_stage: Vec::with_capacity(FIFO_BYTES),
            in_packet: None,
            out: Vec::with_capacity(FIFO_BYTES),
            out_pos: 0,
            capture: Vec::with_capacity(CAPTURE_CAP),
            capture_lost: 0,
            download: false,
            line: (false, false),
            sof: None,
            pending: None,
            drain: None,
            drain_ps: VTime(0),
            enum_delay: VTime(0),
            reset_enum_delay: VTime(0),
            frame: 0,
            irq_driven: false,
            touched: 0,
        };
        m.refresh_status();
        m
    }
}

/// Snapshot form of [`UsjModel`]: `RegStore` carries a `&'static` table and is not serializable,
/// so the section holds the register values and the model's own state.
#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct State {
    regs: Vec<u32>,
    link: HostLink,
    capture_mode: Capture,
    in_stage: Vec<u8>,
    in_packet: Option<Vec<u8>>,
    out: Vec<u8>,
    out_pos: usize,
    capture: Vec<u8>,
    capture_lost: u64,
    download: bool,
    line: (bool, bool),
    sof: Option<EventHandle>,
    pending: Option<EventHandle>,
    drain: Option<EventHandle>,
    drain_ps: VTime,
    enum_delay: VTime,
    reset_enum_delay: VTime,
    frame: u16,
    irq_driven: bool,
    touched: u32,
}

impl Serialize for UsjModel {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        State {
            regs: (0..REG_COUNT).map(|i| self.regs.get(i)).collect(),
            link: self.link,
            capture_mode: self.capture_mode,
            in_stage: self.in_stage.clone(),
            in_packet: self.in_packet.clone(),
            out: self.out.clone(),
            out_pos: self.out_pos,
            capture: self.capture.clone(),
            capture_lost: self.capture_lost,
            download: self.download,
            line: self.line,
            sof: self.sof,
            pending: self.pending,
            drain: self.drain,
            drain_ps: self.drain_ps,
            enum_delay: self.enum_delay,
            reset_enum_delay: self.reset_enum_delay,
            frame: self.frame,
            irq_driven: self.irq_driven,
            touched: self.touched,
        }
        .serialize(s)
    }
}

impl<'de> Deserialize<'de> for UsjModel {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = State::deserialize(d)?;
        if s.out_pos > s.out.len() || s.regs.len() != REG_COUNT {
            return Err(<D::Error as pemu_core::serde::de::Error>::custom(
                "USJ snapshot has the wrong register count or an OUT position past its FIFO",
            ));
        }
        // The FIFOs hold what the model can hold, and no more; anything else is refused, not
        // trimmed.
        if s.in_stage.len() >= FIFO_BYTES
            || s.in_packet
                .as_ref()
                .is_some_and(|p| p.is_empty() || p.len() > FIFO_BYTES)
            || s.out.len() > FIFO_BYTES
        {
            return Err(<D::Error as pemu_core::serde::de::Error>::custom(
                "USJ snapshot has an IN stage, IN packet or OUT FIFO longer than the endpoint holds",
            ));
        }
        let mut m = UsjModel {
            regs: RegStore::new(&REGS),
            link: s.link,
            capture_mode: s.capture_mode,
            in_stage: s.in_stage,
            in_packet: s.in_packet,
            out: s.out,
            out_pos: s.out_pos,
            capture: s.capture,
            capture_lost: s.capture_lost,
            download: s.download,
            line: s.line,
            sof: s.sof,
            pending: s.pending,
            drain: s.drain,
            drain_ps: s.drain_ps,
            enum_delay: s.enum_delay,
            reset_enum_delay: s.reset_enum_delay,
            frame: s.frame,
            irq_driven: s.irq_driven,
            touched: s.touched,
        };
        for (i, v) in s.regs.iter().enumerate().take(REG_COUNT) {
            m.regs.set(i, *v);
        }
        Ok(m)
    }
}

impl UsjModel {
    pub fn link(&self) -> HostLink {
        self.link
    }

    /// Applies the host link from `HostIo::usj_ctrl` and the board rail.
    ///
    /// Applying the link the model already has does nothing: the machine re-applies the journaled
    /// state after unrelated inputs, and re-arming the enumeration delay each time would keep SOF
    /// from ever starting under `device`. A link that becomes enumerated raises `USB_BUS_RESET`
    /// and starts SOF after [`UsjModel::enumeration_delay`]; that delay is model state, never
    /// written back into `link.enumeration`. A link that stops being enumerated stops SOF; the
    /// committed IN packet stays.
    pub fn set_link(&mut self, link: HostLink, now: VTime, sched: &mut Scheduler) {
        if link == self.link {
            return;
        }
        let was = self.link.enumerated();
        self.link = link;
        if !self.link.enumerated() {
            self.cancel_pending(sched);
        } else if !was {
            self.cancel_pending(sched);
            if self.enum_delay.0 > 0 {
                let at = after(now, self.enum_delay);
                self.pending = Some(sched.schedule(now, at, key(TAG_ENUM)));
            } else {
                self.raw_set(INT_BUS_RESET);
            }
        }
        self.arm_sof(now, sched);
        self.arm_drain(now, sched);
        self.refresh_status();
    }

    pub fn enumeration_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn enumeration_delay(&self) -> VTime {
        self.enum_delay
    }

    pub fn set_enumeration_delay(&mut self, delay: VTime) {
        self.enum_delay = delay;
    }

    pub fn reset_enumeration_delay(&self) -> VTime {
        self.reset_enum_delay
    }

    pub fn set_reset_enumeration_delay(&mut self, delay: VTime) {
        self.reset_enum_delay = delay;
    }

    /// Virtual time a committed IN packet waits for the host poll: 0 under `fast`, one host poll
    /// under `device`.
    pub fn drain_interval(&self) -> VTime {
        self.drain_ps
    }

    /// Sets [`UsjModel::drain_interval`]. Going back to an immediate drain takes whatever is
    /// committed at once, so no scheduled event outlives the setting.
    pub fn set_drain_interval(&mut self, delay: VTime, now: VTime, sched: &mut Scheduler) {
        if delay == self.drain_ps {
            return;
        }
        self.drain_ps = delay;
        if let Some(h) = self.drain.take() {
            sched.cancel(h);
        }
        self.arm_drain(now, sched);
        self.refresh_status();
    }

    pub fn drain_pending(&self) -> bool {
        self.drain.is_some()
    }

    pub fn capture_mode(&self) -> Capture {
        self.capture_mode
    }

    pub fn set_capture_mode(&mut self, mode: Capture) {
        self.capture_mode = mode;
    }

    pub fn capture(&self) -> &[u8] {
        &self.capture
    }

    pub fn clear_capture(&mut self) {
        self.capture.clear();
    }

    /// Console bytes the capture buffer had no room for because the wiring pump did not run. A
    /// nonzero count means lost console text.
    pub fn capture_lost(&self) -> u64 {
        self.capture_lost
    }

    /// The download flag; `GPIO_STRAP` latches from it at the reset a (RTS, DTR) = (1, 0) entry
    /// causes.
    pub fn download_flag(&self) -> bool {
        self.download
    }

    pub fn sof_running(&self) -> bool {
        self.sof.is_some()
    }

    pub fn frame_num(&self) -> u16 {
        self.frame
    }

    pub fn out_len(&self) -> usize {
        self.out.len() - self.out_pos
    }

    /// Whether [`UsjModel::push_out_packet`] would take a packet now: a client has the port open
    /// and the OUT FIFO is empty (the next packet is NAKed until firmware drains it). The wiring
    /// asks first, so host bytes are never popped and then dropped.
    pub fn accepts_out(&self) -> bool {
        self.open() && self.out_len() == 0
    }

    pub fn irq_level(&self) -> bool {
        self.int_st() != 0
    }

    /// Drives source 26 to [`UsjModel::irq_level`], touching the fabric only on a change. Every
    /// entry point that can change the level calls it.
    pub fn sync_irq(&mut self, irq: &mut crate::intc::IrqFabric) {
        let level = self.irq_level();
        if level != self.irq_driven {
            self.irq_driven = level;
            irq.set_source(SOURCE, level);
        }
    }

    /// Applies one CDC `SET_CONTROL_LINE_STATE` and reports what the chip does (TRM Table 30.3-2).
    /// A [`LineAction::ChipReset`] is a cause-0x15 reset the machine sequences, latching
    /// `GPIO_STRAP` from its `download` value.
    ///
    /// The whole table is evaluated on every call, so (0, 0) and (0, 1) re-apply the flag each
    /// time. The reset fires only on entry into (1, 0), so esptool's duplicate DTR writes are one
    /// reset.
    pub fn line_state(&mut self, rts: bool, dtr: bool) -> LineAction {
        let repeat = self.line == (rts, dtr);
        self.line = (rts, dtr);
        match (rts, dtr) {
            (false, false) => {
                self.download = false;
                LineAction::DownloadFlag(false)
            }
            (false, true) => {
                self.download = true;
                LineAction::DownloadFlag(true)
            }
            (true, false) if !repeat => LineAction::ChipReset {
                kind: ResetKind::of(ResetCause::USB_UART_CHIP)
                    .expect("cause 0x15 USB_UART_CHIP_RESET is documented"),
                download: self.download,
            },
            _ => LineAction::None,
        }
    }

    /// Delivers one host packet of at most [`FIFO_BYTES`] into the OUT FIFO and returns how many
    /// bytes it took. Only while the FIFO is empty and a client has the port open; otherwise the
    /// host is NAKed and the call returns 0. `SERIAL_OUT_RECV_PKT` is a per-packet event, cleared
    /// only by `INT_CLR`.
    pub fn push_out_packet(&mut self, bytes: &[u8]) -> usize {
        if !self.open() || self.out_len() > 0 {
            return 0;
        }
        let take = bytes.len().min(FIFO_BYTES);
        self.out.clear();
        self.out.extend_from_slice(&bytes[..take]);
        self.out_pos = 0;
        self.raw_set(INT_OUT_RECV_PKT);
        self.refresh_status();
        take
    }
}

fn after(now: VTime, d: VTime) -> VTime {
    VTime(now.0.saturating_add(d.0))
}

impl UsjModel {
    /// `USB_PAD_ENABLE` set and the D+ pull-up not forced off.
    fn pads_on(&self) -> bool {
        let conf0 = self.regs.get(idx::USB_SERIAL_JTAG_CONF0);
        let forced_off = conf0 & CONF0_PAD_PULL_OVERRIDE != 0 && conf0 & CONF0_DP_PULLUP == 0;
        conf0 & CONF0_PAD_ENABLE != 0 && !forced_off
    }

    fn enumerated(&self) -> bool {
        self.link.enumerated() && self.pending.is_none() && self.pads_on()
    }

    fn open(&self) -> bool {
        self.link.open() && self.pending.is_none() && self.pads_on()
    }

    fn arm_sof(&mut self, now: VTime, sched: &mut Scheduler) {
        match (self.enumerated(), self.sof) {
            (true, None) => {
                self.sof = Some(sched.schedule(now, after(now, SOF_PERIOD), key(TAG_SOF)))
            }
            (false, Some(h)) => {
                sched.cancel(h);
                self.sof = None;
            }
            _ => {}
        }
    }

    fn cancel_pending(&mut self, sched: &mut Scheduler) {
        if let Some(h) = self.pending.take() {
            sched.cancel(h);
        }
    }

    /// RAW is `Ro` in the generated table, so this uses `RegStore::set`.
    fn raw_set(&mut self, bits: u32) {
        let raw = self.regs.get(idx::USB_SERIAL_JTAG_INT_RAW) | bits;
        self.regs.set(idx::USB_SERIAL_JTAG_INT_RAW, raw);
    }

    fn int_st(&self) -> u32 {
        self.regs.get(idx::USB_SERIAL_JTAG_INT_RAW) & self.regs.get(idx::USB_SERIAL_JTAG_INT_ENA)
    }

    fn in_data_free(&self) -> bool {
        self.in_packet.is_none() && self.in_stage.len() < FIFO_BYTES
    }

    /// An undrained packet keeps `SERIAL_IN_EMPTY` at 0.
    fn in_empty(&self) -> bool {
        self.in_packet.is_none() && self.in_stage.is_empty()
    }

    /// Re-evaluates everything that is a function of state rather than an event: the `EP1_CONF`
    /// status bits, the level-like raw bit, `INT_ST` and the endpoint status registers.
    ///
    /// `SERIAL_IN_EMPTY` is the only level-like raw bit. The others are events and are not
    /// re-asserted: an ISR that clears `SERIAL_OUT_RECV_PKT` without draining must not see it
    /// come back (an interrupt storm), and a re-asserted `SOF` would make the connection monitor
    /// read "connected" after the cable is gone.
    fn refresh_status(&mut self) {
        // With an immediate drain a committed packet is taken as soon as the host can, so opening
        // the port or re-enabling the pads drains what U2 or U0 left behind. With a drain interval
        // the packet waits for its event (`arm_drain`).
        if self.drain_ps.0 == 0 {
            self.host_takes();
        }
        let mut conf = self.regs.get(idx::USB_SERIAL_JTAG_EP1_CONF)
            & !(EP1_CONF_IN_DATA_FREE | EP1_CONF_OUT_DATA_AVAIL);
        if self.in_data_free() {
            conf |= EP1_CONF_IN_DATA_FREE;
        }
        if self.out_len() > 0 {
            conf |= EP1_CONF_OUT_DATA_AVAIL;
        }
        self.regs.set(idx::USB_SERIAL_JTAG_EP1_CONF, conf);

        let mut raw = self.regs.get(idx::USB_SERIAL_JTAG_INT_RAW) & !INT_IN_EMPTY;
        if self.in_empty() {
            raw |= INT_IN_EMPTY;
        }
        raw &= INT_MODELED;
        self.regs.set(idx::USB_SERIAL_JTAG_INT_RAW, raw);
        let st = raw & self.regs.get(idx::USB_SERIAL_JTAG_INT_ENA);
        self.regs.set(idx::USB_SERIAL_JTAG_INT_ST, st);

        let cnt = (self.out_len() as u32) << 16;
        self.regs.set(idx::USB_SERIAL_JTAG_OUT_EP1_ST, cnt);
        self.regs
            .set(idx::USB_SERIAL_JTAG_FRAM_NUM, u32::from(self.frame));
    }

    fn record(&mut self, bytes: &[u8]) {
        let room = CAPTURE_CAP.saturating_sub(self.capture.len());
        let take = room.min(bytes.len());
        self.capture.extend_from_slice(&bytes[..take]);
        self.capture_lost += (bytes.len() - take) as u64;
    }

    /// Commits the staged bytes as one IN packet. An empty stage commits nothing: the zero-length
    /// packet IDF sends after an exactly-full one is ignored.
    fn commit(&mut self, now: VTime, sched: &mut Scheduler) {
        if self.in_stage.is_empty() {
            return;
        }
        let packet = core::mem::take(&mut self.in_stage);
        if self.capture_mode == Capture::Fifo {
            self.record(&packet);
        }
        self.in_packet = Some(packet);
        if self.drain_ps.0 == 0 {
            self.host_takes();
        } else {
            self.arm_drain(now, sched);
        }
    }

    /// Arms the host poll that will take the committed packet, only while a client has the port
    /// open; the entry points that open it arm it then.
    fn arm_drain(&mut self, now: VTime, sched: &mut Scheduler) {
        if self.drain.is_some() || self.drain_ps.0 == 0 || self.in_packet.is_none() || !self.open()
        {
            return;
        }
        self.drain = Some(sched.schedule(now, after(now, self.drain_ps), key(TAG_DRAIN)));
    }

    /// The host takes the committed packet (U3 only). In `wire` mode this is where the bytes
    /// enter the capture.
    fn host_takes(&mut self) {
        if !self.open() {
            return;
        }
        let Some(packet) = self.in_packet.take() else {
            return;
        };
        if self.capture_mode == Capture::Wire {
            self.record(&packet);
        }
        self.raw_set(INT_IN_TOKEN_REC_EP1);
    }

    /// A guest write of `EP1.RDWR_BYTE`: append while `SERIAL_IN_EP_DATA_FREE`, commit at
    /// [`FIFO_BYTES`]. A byte written with the endpoint full is dropped, which the ROM's bounded
    /// poll relies on.
    fn write_ep1(&mut self, byte: u8, now: VTime, sched: &mut Scheduler) {
        if !self.in_data_free() {
            return;
        }
        self.in_stage.push(byte);
        if self.in_stage.len() == FIFO_BYTES {
            self.commit(now, sched);
        }
    }

    /// A guest read of `EP1.RDWR_BYTE`: pop one byte from the OUT FIFO, or 0 when it is empty
    /// (UNVERIFIED; the ROM checks `SERIAL_OUT_EP_DATA_AVAIL` first).
    fn read_ep1(&mut self) -> u32 {
        if self.out_pos >= self.out.len() {
            return 0;
        }
        let byte = self.out[self.out_pos];
        self.out_pos += 1;
        if self.out_pos >= self.out.len() {
            self.out.clear();
            self.out_pos = 0;
        }
        u32::from(byte)
    }
}

/// Register offsets this model branches on; `tests::offsets_match_the_generated_table` pins them.
mod off {
    pub const EP1: u32 = 0x000;
    pub const EP1_CONF: u32 = 0x004;
    pub const INT_RAW: u32 = 0x008;
    pub const INT_ST: u32 = 0x00C;
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "register map; only the tests write it")
    )]
    pub const INT_ENA: u32 = 0x010;
    pub const INT_CLR: u32 = 0x014;
    pub const CONF0: u32 = 0x018;
    pub const FRAM_NUM: u32 = 0x024;
}

impl UsjModel {
    fn touch(
        &mut self,
        reg: usize,
        off: u32,
        access: TouchAccess,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) {
        let bit = 1u32 << reg;
        if self.touched & bit != 0 {
            return;
        }
        self.touched |= bit;
        ledger.first_touch(FirstTouch {
            periph: <Self as Peripheral>::ID,
            off,
            access,
            size: size as u8,
            now,
            allowlisted: false,
        });
    }
}

impl UsjModel {
    /// Reads `size` bytes at `off`. An offset with no register reads 0. `EP1` pops a byte from the
    /// OUT FIFO; a narrow read not covering byte 0 pops nothing, because IDF always reads the
    /// register whole.
    pub fn reg_read(
        &mut self,
        off: u32,
        size: Size,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> u32 {
        let reg_off = off & !3;
        let byte_off = (off & 3) as u8;
        let Some(reg) = self.regs.index_of(reg_off as u16) else {
            return 0;
        };
        self.touch(reg, reg_off, TouchAccess::Read, size, now, ledger);
        if reg_off == off::INT_ST {
            let st = self.int_st();
            self.regs.set(idx::USB_SERIAL_JTAG_INT_ST, st);
        }
        if reg_off == off::EP1 && byte_off == 0 {
            let byte = self.read_ep1();
            self.regs.set(idx::USB_SERIAL_JTAG_EP1, byte);
            self.refresh_status();
        }
        self.regs.read(reg, byte_off, size)
    }

    pub fn reg_write(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        sched: &mut Scheduler,
        ledger: &mut FidelityLedger,
    ) -> Wiring {
        let reg_off = off & !3;
        let byte_off = (off & 3) as u8;
        let Some(reg) = self.regs.index_of(reg_off as u16) else {
            return Wiring::None;
        };
        self.touch(reg, reg_off, TouchAccess::Write, size, now, ledger);
        let delta = self.regs.write(reg, byte_off, size, val);
        let mut wiring = Wiring::None;
        match reg_off {
            off::EP1 if byte_off == 0 => {
                self.write_ep1(val as u8, now, sched);
                wiring = Wiring::UsjIo;
            }
            off::EP1_CONF => {
                if delta.triggers & EP1_CONF_WR_DONE != 0 {
                    self.commit(now, sched);
                    wiring = Wiring::UsjIo;
                }
            }
            off::INT_CLR => {
                let raw = self.regs.get(idx::USB_SERIAL_JTAG_INT_RAW) & !delta.triggers;
                self.regs.set(idx::USB_SERIAL_JTAG_INT_RAW, raw);
            }
            off::CONF0 => {
                // A pad or pull-up change is a connect or disconnect to the host, so SOF is
                // re-armed and the wiring pump re-runs.
                self.arm_sof(now, sched);
                self.arm_drain(now, sched);
                wiring = Wiring::UsjIo;
            }
            _ => {}
        }
        self.refresh_status();
        wiring
    }

    /// One of this block's timers fired: the SOF tick, the enumeration delay or the IN drain.
    ///
    /// Both SOF and enumeration ask for the ring pump. The SOF tick is the only thing that keeps
    /// host-to-guest data moving while the ROM download path only reads, so without it
    /// `write_flash` would stop after the first 64-byte packet.
    ///
    /// UNVERIFIED rate: one OUT packet per emulated millisecond, 64 kB/s. Real hardware accepts the
    /// next packet as soon as the FIFO is drained.
    pub fn tick(&mut self, tag: u16, now: VTime, sched: &mut Scheduler) -> Wiring {
        let mut wiring = Wiring::None;
        match tag {
            TAG_SOF => {
                self.sof = None;
                self.frame = (self.frame + 1) & FRAME_MASK;
                self.raw_set(INT_SOF);
                self.arm_sof(now, sched);
                self.arm_drain(now, sched);
                wiring = Wiring::UsjIo;
            }
            TAG_ENUM => {
                self.pending = None;
                self.raw_set(INT_BUS_RESET);
                self.arm_sof(now, sched);
                self.arm_drain(now, sched);
                wiring = Wiring::UsjIo;
            }
            TAG_DRAIN => {
                self.drain = None;
                // Nothing to arm: the next poll is armed by the next commit, so a burst costs one
                // poll per packet.
                self.host_takes();
                wiring = Wiring::UsjIo;
            }
            _ => {}
        }
        self.refresh_status();
        wiring
    }

    /// How long a read of `off` keeps its value. The three registers the SOF tick moves hold
    /// only until the next event while SOF runs; every other read changes only with host input.
    pub fn stability(&self, off: u32) -> Stability {
        match off & !3 {
            // The IN drain frees the endpoint at a scheduled event, so the ROM's poll can
            // fast-forward to it.
            off::EP1_CONF if self.drain.is_some() => Stability::UntilNextEvent,
            off::INT_RAW | off::INT_ST | off::FRAM_NUM
                if self.sof.is_some() || self.drain.is_some() =>
            {
                Stability::UntilNextEvent
            }
            _ => Stability::UntilInput,
        }
    }

    /// Restores the register file for `kind` and empties both endpoints, keeping the captured
    /// console bytes.
    ///
    /// The host link and a running enumeration delay are outside the SoC and keep their state:
    /// esptool holds (RTS, DTR) = (1, 0) for 0.1 to 0.2 s, so a cause-0x15 reset lands inside the
    /// enumeration window, and cancelling the delay would kill the console. A `SYS_` reset (0x0F,
    /// 0x10, 0x12, 0x13) resets the RTC domain too: the link drops and SOF resumes
    /// [`UsjModel::reset_enumeration_delay`] later. Class B for 0x12 (the `probe_campaign_reset`
    /// capture shows the link down 296 ms, while the 0x15 `CORE_` reset kept it); UNVERIFIED for
    /// the others.
    ///
    /// The download flag and the last line state are chip state: only a power cycle clears them,
    /// so the flag (0, 1) sets survives the reset (1, 0) causes. Clearing the line latch with it
    /// is UNVERIFIED.
    pub fn reset_to(&mut self, kind: ResetKind, now: VTime, sched: &mut Scheduler) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.in_stage.clear();
        self.in_packet = None;
        self.out.clear();
        self.out_pos = 0;
        self.frame = 0;
        if kind.scope == ResetScope::Chip {
            self.download = false;
            self.line = (false, false);
        }
        if kind.scope == ResetScope::System && self.link.enumerated() {
            // A `SYS_` reset drops the USB link and the host enumerates again: SOF stops for
            // `reset_enum_delay`.
            self.cancel_pending(sched);
            if self.reset_enum_delay.0 > 0 {
                let at = after(now, self.reset_enum_delay);
                self.pending = Some(sched.schedule(now, at, key(TAG_ENUM)));
            } else {
                self.raw_set(INT_BUS_RESET);
            }
        }
        if let Some(h) = self.sof.take() {
            sched.cancel(h);
        }
        if let Some(h) = self.drain.take() {
            sched.cancel(h);
        }
        self.arm_sof(now, sched);
        self.refresh_status();
    }
}

impl Peripheral for UsjModel {
    const ID: PeriphId = <block::Usj as Block>::ID;
    const BASE: u32 = <block::Usj as Block>::BASE;
    const SIZE: u32 = <block::Usj as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_to(kind, cx.now, cx.sched);
        self.sync_irq(cx.irq);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        let val = self.reg_read(off, size, cx.now, cx.ledger);
        self.sync_irq(cx.irq);
        RegRead { val, stop: false }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.reg_write(off, size, val, cx.now, cx.sched, cx.ledger);
        self.sync_irq(cx.irq);
        RegWrite {
            stop: false,
            wiring,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        let wiring = self.tick(tag, cx.now, cx.sched);
        self.sync_irq(cx.irq);
        wiring
    }

    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        self.stability(off)
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        self.regs
            .index_of((off & !3) as u16)
            .map_or(Fidelity::U, |reg| self.regs.spec(reg).class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::sched::EventKey;

    #[test]
    fn a_usj_snapshot_with_an_oversized_fifo_is_refused() {
        use pemu_core::snap::{SectionId, serde_from_section, serde_section};
        let decode = |edit: &dyn Fn(&mut State)| {
            let m = UsjModel::default();
            let mut state = State {
                regs: (0..REG_COUNT).map(|i| m.regs.get(i)).collect(),
                link: m.link,
                capture_mode: m.capture_mode,
                in_stage: Vec::new(),
                in_packet: None,
                out: Vec::new(),
                out_pos: 0,
                capture: Vec::new(),
                capture_lost: 0,
                download: false,
                line: (false, false),
                sof: None,
                pending: None,
                drain: None,
                drain_ps: VTime(0),
                enum_delay: VTime(0),
                reset_enum_delay: VTime(0),
                frame: 0,
                irq_driven: false,
                touched: 0,
            };
            edit(&mut state);
            let section = serde_section(&state, 1, "test").expect("encodes");
            serde_from_section::<UsjModel>(&section, SectionId::soc("usj"), 1, "test")
        };
        assert!(decode(&|s| s.in_stage = vec![1; FIFO_BYTES - 1]).is_ok());
        assert!(decode(&|s| s.in_packet = Some(vec![1; FIFO_BYTES])).is_ok());
        assert!(decode(&|s| s.out = vec![1; FIFO_BYTES]).is_ok());
        assert!(decode(&|s| s.in_stage = vec![1; FIFO_BYTES]).is_err());
        assert!(decode(&|s| s.in_packet = Some(vec![1; FIFO_BYTES + 1])).is_err());
        assert!(decode(&|s| s.in_packet = Some(Vec::new())).is_err());
        assert!(decode(&|s| s.out = vec![1; FIFO_BYTES + 1]).is_err());
    }

    struct Rig {
        m: UsjModel,
        sched: Scheduler,
        ledger: FidelityLedger,
        now: VTime,
    }

    impl Rig {
        /// A model in U3 after the power-on reset, which arms the SOF tick.
        fn open() -> Rig {
            let mut rig = Rig {
                m: UsjModel::default(),
                sched: Scheduler::new(),
                ledger: FidelityLedger::default(),
                now: VTime(0),
            };
            rig.power_on();
            rig
        }

        fn power_on(&mut self) {
            let kind = ResetKind::of(ResetCause::POWERON).expect("cause 0x01 is documented");
            self.m.reset_to(kind, self.now, &mut self.sched);
        }

        fn link(&mut self, link: HostLink) {
            self.m.set_link(link, self.now, &mut self.sched);
        }

        fn drain_after(&mut self, ps: u64) {
            self.m
                .set_drain_interval(VTime(ps), self.now, &mut self.sched);
        }

        fn read(&mut self, off: u32) -> u32 {
            self.m.reg_read(off, Size::B4, self.now, &mut self.ledger)
        }

        fn write(&mut self, off: u32, val: u32) -> Wiring {
            self.m.reg_write(
                off,
                Size::B4,
                val,
                self.now,
                &mut self.sched,
                &mut self.ledger,
            )
        }

        /// One `EP1.RDWR_BYTE` write, as ROM `usb_uart_tx_one_char` and the IDF VFS do.
        fn put(&mut self, byte: u8) {
            self.write(off::EP1, u32::from(byte));
        }

        /// Bytes plus the `WR_DONE` flush of ROM `usb_uart_tx_flush`.
        fn puts(&mut self, bytes: &[u8]) {
            for b in bytes {
                self.put(*b);
            }
            self.write(off::EP1_CONF, EP1_CONF_WR_DONE);
        }

        fn advance(&mut self, ps: u64) {
            let target = VTime(self.now.0.saturating_add(ps));
            while let Some(t) = self.sched.next_time() {
                if t > target {
                    break;
                }
                self.now = t;
                while let Some(EventKey { tag, .. }) = self.sched.pop_due(self.now) {
                    self.m.tick(tag, self.now, &mut self.sched);
                }
            }
            self.now = target;
        }
    }

    /// Under `device` the committed packet waits for one host poll, `SERIAL_IN_EP_DATA_FREE` reads
    /// 0 until it, and the bytes reach the capture at the poll.
    #[test]
    fn a_committed_packet_waits_one_host_poll_before_the_capture_sees_it() {
        const POLL_PS: u64 = 125_000_000;
        let mut rig = Rig::open();
        rig.drain_after(POLL_PS);
        let t0 = rig.now;
        rig.puts(b"boot:0xa\r\n");

        assert!(rig.m.drain_pending(), "the host poll is armed");
        assert_eq!(
            rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE,
            0,
            "the endpoint is full until the host takes the packet"
        );
        assert!(rig.m.capture().is_empty(), "and nothing is on the wire yet");
        assert!(
            matches!(rig.m.stability(off::EP1_CONF), Stability::UntilNextEvent),
            "so the ROM's bounded poll can fast-forward to the drain"
        );

        rig.advance(POLL_PS - 1);
        assert!(rig.m.capture().is_empty(), "not a picosecond early");

        rig.advance(1);
        assert_eq!(rig.now, VTime(t0.0 + POLL_PS));
        assert_eq!(rig.m.capture(), b"boot:0xa\r\n");
        assert!(!rig.m.drain_pending());
        assert_ne!(rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE, 0);
    }

    /// Back-to-back packets cost one host poll each, which is what makes a printed line cost its
    /// transmission time.
    #[test]
    fn back_to_back_packets_cost_one_host_poll_each() {
        const POLL_PS: u64 = 125_000_000;
        let mut rig = Rig::open();
        rig.drain_after(POLL_PS);
        let t0 = rig.now;
        for i in 0..4u8 {
            rig.puts(&[b'a' + i]);
            // A write with the endpoint full loses the byte, so the guest waits.
            rig.advance(POLL_PS);
        }
        assert_eq!(rig.m.capture(), b"abcd");
        assert_eq!(rig.now, VTime(t0.0 + 4 * POLL_PS));
    }

    /// The drain runs only in U3: a packet committed with no client waits, and the poll is armed
    /// by whatever opens the port.
    #[test]
    fn a_packet_committed_without_a_client_drains_when_the_port_opens() {
        const POLL_PS: u64 = 125_000_000;
        let mut rig = Rig::open();
        rig.drain_after(POLL_PS);
        rig.link(HostLink::IDLE);
        rig.puts(b"hi");
        assert!(!rig.m.drain_pending(), "U2 takes no IN packet");
        rig.advance(10 * POLL_PS);
        assert!(rig.m.capture().is_empty());

        rig.link(HostLink::OPEN);
        assert!(rig.m.drain_pending(), "opening the port arms the poll");
        rig.advance(POLL_PS);
        assert_eq!(rig.m.capture(), b"hi");
    }

    /// Going back to the immediate drain takes what is committed at once and leaves no event.
    #[test]
    fn dropping_the_drain_interval_takes_the_packet_at_once() {
        const POLL_PS: u64 = 125_000_000;
        let mut rig = Rig::open();
        rig.drain_after(POLL_PS);
        rig.puts(b"z");
        assert!(rig.m.drain_pending());
        rig.drain_after(0);
        assert!(!rig.m.drain_pending());
        assert_eq!(rig.m.capture(), b"z");
        assert_eq!(rig.m.drain_interval(), VTime(0));
    }

    #[test]
    fn a_reset_cancels_the_pending_host_poll() {
        const POLL_PS: u64 = 125_000_000;
        let mut rig = Rig::open();
        rig.drain_after(POLL_PS);
        rig.puts(b"gone");
        let before = rig.sched.len();
        rig.power_on();
        assert!(!rig.m.drain_pending());
        assert!(rig.sched.len() < before);
        assert_eq!(rig.m.drain_interval(), VTime(POLL_PS));
        rig.puts(b"kept");
        rig.advance(POLL_PS);
        assert_eq!(rig.m.capture(), b"kept", "the block works after the reset");
    }

    /// The IDF connection monitor: a tick hook that samples and clears the `SOF` raw bit, and
    /// reports the port disconnected after `ALLOWED_NO_SOF_TICKS` = 3 ticks without one.
    ///
    /// The replica disconnects on the third missed tick. UNVERIFIED off-by-one:
    /// `specs/blocks/usj.toml` says 4. Nothing depends on it, because the model stops SOF the
    /// instant the link goes down.
    struct SofMonitor {
        missed: u32,
        connected: bool,
    }

    impl SofMonitor {
        const ALLOWED: u32 = 3;

        fn new() -> SofMonitor {
            SofMonitor {
                missed: 0,
                connected: true,
            }
        }

        fn tick(&mut self, rig: &mut Rig) {
            if rig.read(off::INT_RAW) & INT_SOF != 0 {
                rig.write(off::INT_CLR, INT_SOF);
                self.missed = 0;
                self.connected = true;
            } else {
                self.missed += 1;
                if self.missed >= SofMonitor::ALLOWED {
                    self.connected = false;
                }
            }
        }
    }

    #[test]
    fn sof_keeps_the_console_alive_and_stopping_sof_kills_it() {
        let mut rig = Rig::open();
        let mut mon = SofMonitor::new();
        assert!(rig.m.sof_running(), "U3 runs SOF");
        rig.advance(SOF_PERIOD.0 / 2);
        for tick in 1..=10 {
            rig.advance(SOF_PERIOD.0);
            mon.tick(&mut rig);
            assert!(mon.connected, "tick {tick} saw a SOF");
            assert_eq!(rig.m.frame_num(), tick, "FRAME_NUM advances once per SOF");
        }
        rig.puts(b"alive\n");
        assert_eq!(rig.m.capture(), b"alive\n", "the console reaches the host");
        rig.m.clear_capture();

        rig.link(HostLink::DETACHED);
        assert!(!rig.m.sof_running());
        let frozen = rig.m.frame_num();
        for missed in 1..=6 {
            rig.advance(SOF_PERIOD.0);
            mon.tick(&mut rig);
            assert_eq!(
                mon.connected,
                missed < SofMonitor::ALLOWED,
                "the monitor disconnects on missed tick {missed}",
            );
        }
        assert_eq!(
            rig.m.frame_num(),
            frozen,
            "FRAME_NUM is frozen while detached"
        );
        rig.puts(b"lost\n");
        assert!(
            rig.m.capture().is_empty(),
            "with no host the console text never leaves the endpoint",
        );

        rig.link(HostLink::OPEN);
        assert!(rig.m.sof_running());
        rig.advance(SOF_PERIOD.0);
        mon.tick(&mut rig);
        assert!(mon.connected, "SOF is back, so the console is back");
    }

    const OFF_OUT_EP1_ST: u32 = 0x03C;

    #[test]
    fn the_drain_rules_follow_the_host_state() {
        let mut rig = Rig::open();
        rig.write(off::INT_CLR, INT_IN_TOKEN_REC_EP1);
        rig.puts(b"hi");
        assert_eq!(rig.m.capture(), b"hi");
        assert_ne!(rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE, 0);
        assert_ne!(rig.read(off::INT_RAW) & INT_IN_EMPTY, 0);
        assert_ne!(rig.read(off::INT_RAW) & INT_IN_TOKEN_REC_EP1, 0);

        // U2: SOF runs, so the console is "connected", but nothing reads the port; the packet
        // stays and `SERIAL_IN_EMPTY` stays 0.
        let mut rig = Rig::open();
        rig.link(HostLink::IDLE);
        rig.write(off::INT_CLR, INT_IN_EMPTY | INT_IN_TOKEN_REC_EP1);
        rig.puts(b"hi");
        assert!(rig.m.sof_running());
        assert!(rig.m.capture().is_empty());
        assert_eq!(rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE, 0);
        assert_eq!(rig.read(off::INT_RAW) & INT_IN_EMPTY, 0);
        assert_eq!(rig.read(off::INT_RAW) & INT_IN_TOKEN_REC_EP1, 0);
        // Further bytes are dropped rather than queued, which is why the ROM's bounded poll loses
        // characters instead of hanging.
        rig.puts(b"dropped");
        // Opening the port lets the host take what was waiting, and only that.
        rig.link(HostLink::OPEN);
        assert_eq!(rig.m.capture(), b"hi");

        let mut rig = Rig::open();
        rig.link(HostLink::DETACHED);
        rig.puts(b"hi");
        assert!(!rig.m.sof_running());
        assert!(rig.m.capture().is_empty());
        assert_eq!(rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE, 0);
    }

    #[test]
    fn packets_commit_on_wr_done_and_at_sixty_four_bytes() {
        let mut rig = Rig::open();
        for b in b"short" {
            rig.put(*b);
        }
        assert!(rig.m.capture().is_empty(), "nothing leaves before WR_DONE");
        assert!(matches!(
            rig.write(off::EP1_CONF, EP1_CONF_WR_DONE),
            Wiring::UsjIo
        ));
        assert_eq!(rig.m.capture(), b"short");
        rig.m.clear_capture();
        rig.write(off::EP1_CONF, EP1_CONF_WR_DONE);
        assert!(rig.m.capture().is_empty());

        let full: Vec<u8> = (0..FIFO_BYTES as u8).collect();
        for b in &full {
            rig.put(*b);
        }
        assert_eq!(rig.m.capture(), &full[..], "64 bytes commit on their own");
        rig.m.clear_capture();
        rig.write(off::EP1_CONF, EP1_CONF_WR_DONE);
        assert!(rig.m.capture().is_empty());
    }

    #[test]
    fn the_out_fifo_takes_one_packet_at_a_time() {
        let mut rig = Rig::open();
        assert_eq!(rig.read(off::EP1_CONF) & EP1_CONF_OUT_DATA_AVAIL, 0);
        assert_eq!(rig.read(off::INT_RAW) & INT_OUT_RECV_PKT, 0);

        assert_eq!(rig.m.push_out_packet(b"AB"), 2);
        assert_ne!(rig.read(off::EP1_CONF) & EP1_CONF_OUT_DATA_AVAIL, 0);
        assert_ne!(rig.read(off::INT_RAW) & INT_OUT_RECV_PKT, 0);
        assert_eq!(rig.read(OFF_OUT_EP1_ST) >> 16, 2, "OUT_EP1_REC_DATA_CNT");
        assert_eq!(rig.m.push_out_packet(b"C"), 0, "the host is NAKed");
        assert!(!rig.m.accepts_out());

        assert_eq!(rig.read(off::EP1), u32::from(b'A'));
        assert_eq!(rig.read(off::EP1), u32::from(b'B'));
        assert_eq!(rig.read(off::EP1_CONF) & EP1_CONF_OUT_DATA_AVAIL, 0);
        assert_eq!(
            rig.read(off::EP1),
            0,
            "an empty OUT FIFO reads 0 (UNVERIFIED)"
        );

        // `SERIAL_OUT_RECV_PKT` is a per-packet event: draining does not clear it, `INT_CLR`
        // does, and it does not come back, so an ISR that clears it without draining sees no
        // interrupt storm.
        assert_ne!(
            rig.read(off::INT_RAW) & INT_OUT_RECV_PKT,
            0,
            "still latched"
        );
        rig.write(off::INT_CLR, INT_OUT_RECV_PKT);
        assert_eq!(rig.read(off::INT_RAW) & INT_OUT_RECV_PKT, 0);
        assert_eq!(rig.m.push_out_packet(b"CD"), 2);
        assert_ne!(rig.read(off::INT_RAW) & INT_OUT_RECV_PKT, 0, "next packet");
        rig.write(off::INT_CLR, INT_OUT_RECV_PKT);
        assert_eq!(
            rig.read(off::INT_RAW) & INT_OUT_RECV_PKT,
            0,
            "cleared with the FIFO still full, it stays cleared",
        );
        assert_ne!(rig.read(off::EP1_CONF) & EP1_CONF_OUT_DATA_AVAIL, 0);
        assert_eq!(rig.read(off::EP1), u32::from(b'C'));
        assert_eq!(rig.read(off::EP1), u32::from(b'D'));

        let long: Vec<u8> = (0..100u8).collect();
        assert_eq!(
            rig.m.push_out_packet(&long),
            FIFO_BYTES,
            "packets cap at 64"
        );

        let mut rig = Rig::open();
        rig.link(HostLink::IDLE);
        assert!(!rig.m.accepts_out());
        assert_eq!(rig.m.push_out_packet(b"Z"), 0);
    }

    #[test]
    fn interrupts_gate_on_int_ena_and_the_level_like_raw_bits_come_back() {
        let mut rig = Rig::open();
        assert!(!rig.m.irq_level(), "INT_ENA is 0 after reset");
        rig.advance(SOF_PERIOD.0);
        assert_ne!(rig.read(off::INT_RAW) & INT_SOF, 0, "SOF ignores INT_ENA");
        assert_eq!(rig.read(off::INT_ST), 0);
        assert!(!rig.m.irq_level());

        rig.write(off::INT_ENA, INT_SOF);
        assert_eq!(rig.read(off::INT_ST), INT_SOF);
        assert!(rig.m.irq_level(), "source 26 is high");
        rig.write(off::INT_CLR, INT_SOF);
        assert_eq!(rig.read(off::INT_RAW) & INT_SOF, 0);
        assert!(!rig.m.irq_level(), "SOF is an event, not a level");

        // `SERIAL_IN_EMPTY` is level-like: cleared while the FIFO is still empty, it is set again.
        // An edge-only bit stalls the Passport Keys interrupt-driven TX.
        rig.write(off::INT_ENA, INT_IN_EMPTY);
        rig.write(off::INT_CLR, INT_IN_EMPTY);
        assert_ne!(rig.read(off::INT_RAW) & INT_IN_EMPTY, 0);
        assert!(rig.m.irq_level());

        // With an undrained packet the bit is genuinely 0, so an idle host causes no storm.
        rig.link(HostLink::IDLE);
        rig.puts(b"x");
        assert_eq!(rig.read(off::INT_RAW) & INT_IN_EMPTY, 0);
        assert!(!rig.m.irq_level());
        rig.write(off::INT_CLR, INT_IN_EMPTY);
        assert_eq!(rig.read(off::INT_RAW) & INT_IN_EMPTY, 0);
    }

    #[test]
    fn the_enumeration_delay_holds_sof_back() {
        let mut rig = Rig::open();
        rig.link(HostLink::DETACHED);
        rig.write(off::INT_CLR, INT_BUS_RESET);
        rig.m.set_enumeration_delay(VTime(100_000_000_000));
        rig.link(HostLink::OPEN);
        assert!(!rig.m.sof_running(), "the delay is still running");
        assert_eq!(rig.read(off::INT_RAW) & INT_BUS_RESET, 0);
        rig.advance(99_000_000_000);
        assert!(!rig.m.sof_running());
        rig.advance(2_000_000_000);
        assert!(rig.m.sof_running(), "SOF starts when the delay elapses");
        assert_ne!(
            rig.read(off::INT_RAW) & INT_BUS_RESET,
            0,
            "enumeration raises USB_BUS_RESET",
        );
    }

    /// The `probe_campaign_reset` capture: the super-watchdog reset (0x12, `SYS_`) drops the link
    /// until [`UsjModel::reset_enumeration_delay`], while a `CORE_` reset (0x15) keeps SOF.
    #[test]
    fn a_sys_reset_drops_the_link_for_the_reset_enumeration_delay() {
        let mut rig = Rig::open();
        rig.m.set_enumeration_delay(VTime(1));
        rig.m.set_reset_enumeration_delay(VTime(200_000_000_000));
        assert!(rig.m.sof_running());

        let core = ResetKind::of(ResetCause::USB_UART_CHIP).expect("cause 0x15 is documented");
        rig.m.reset_to(core, rig.now, &mut rig.sched);
        assert!(rig.m.sof_running(), "a CORE_ reset keeps the link");
        assert!(!rig.m.enumeration_pending());

        let sys = ResetKind::of(ResetCause::SUPER_WDT).expect("cause 0x12 is documented");
        rig.m.reset_to(sys, rig.now, &mut rig.sched);
        assert!(!rig.m.sof_running(), "a SYS_ reset drops it");
        assert!(rig.m.enumeration_pending());
        assert_eq!(rig.m.link(), HostLink::OPEN, "the host side did not change");
        rig.advance(199_000_000_000);
        assert!(!rig.m.sof_running(), "the reset delay, not the plug delay");
        rig.advance(2_000_000_000);
        assert!(rig.m.sof_running(), "SOF comes back after the reset delay");
        assert_ne!(rig.read(off::INT_RAW) & INT_BUS_RESET, 0);

        rig.link(HostLink::DETACHED);
        rig.m.reset_to(sys, rig.now, &mut rig.sched);
        assert!(
            !rig.m.enumeration_pending(),
            "no link, nothing to enumerate"
        );
    }

    /// A reset taken during the enumeration delay does not lose it. esptool's cause-0x15 reset
    /// lands inside the 100 ms `device` window; dropping the delay would leave SOF armed by
    /// nothing and the monitor would drop every console byte for the rest of the run.
    #[test]
    fn a_reset_during_the_enumeration_delay_still_brings_sof_back() {
        let mut rig = Rig::open();
        rig.m.set_enumeration_delay(VTime(100_000_000_000));
        rig.link(HostLink::DETACHED);
        rig.link(HostLink::OPEN);
        assert!(rig.m.enumeration_pending(), "the delay is running");
        rig.advance(50_000_000_000);

        let kind = ResetKind::of(ResetCause::USB_UART_CHIP).expect("cause 0x15 is documented");
        rig.m.reset_to(kind, rig.now, &mut rig.sched);
        assert!(!rig.m.sof_running(), "the delay has not elapsed yet");
        assert!(rig.m.enumeration_pending(), "and the reset did not drop it");
        assert_eq!(
            rig.m.link(),
            HostLink::OPEN,
            "the link reports what the host said, not the model's latch",
        );

        rig.advance(50_000_000_000);
        assert!(rig.m.sof_running(), "SOF comes back when the delay elapses");
        rig.advance(10 * SOF_PERIOD.0);
        assert_eq!(rig.m.frame_num(), 10, "and FRAME_NUM advances again");
    }

    /// Applying the same host state twice does nothing. `wiring/usj.rs::apply_ctrl` re-applies the
    /// journaled state after any input, so restarting the delay on every call would starve SOF.
    #[test]
    fn re_applying_the_same_link_does_not_restart_the_enumeration_delay() {
        let mut rig = Rig::open();
        rig.m.set_enumeration_delay(VTime(100_000_000_000));
        rig.link(HostLink::DETACHED);
        rig.link(HostLink::OPEN);
        for _ in 0..20 {
            rig.advance(50_000_000_000);
            rig.link(HostLink::OPEN);
        }
        assert!(rig.m.sof_running(), "SOF started 100 ms after the plug");
        assert!(!rig.m.enumeration_pending());
        assert!(rig.m.frame_num() > 0);
    }

    /// The line-state table (TRM Table 30.3-2).
    #[test]
    fn the_line_state_table_sets_the_flag_and_resets_with_cause_0x15() {
        let mut rig = Rig::open();
        assert!(!rig.m.download_flag());
        assert_eq!(rig.m.line_state(true, true), LineAction::None);
        assert_eq!(
            rig.m.line_state(false, true),
            LineAction::DownloadFlag(true)
        );
        assert!(rig.m.download_flag());
        let LineAction::ChipReset { kind, download } = rig.m.line_state(true, false) else {
            panic!("(RTS, DTR) = (1, 0) resets the chip");
        };
        assert_eq!(kind.cause, ResetCause::USB_UART_CHIP);
        assert_eq!(kind.cause.0, 0x15);
        assert!(
            download,
            "esptool set the flag first, so this boots download mode"
        );
        // esptool writes RTS twice for `usbser.sys`; the repeat is not a second reset.
        assert_eq!(rig.m.line_state(true, false), LineAction::None);
        // (0, 0): clear the flag, so the next RTS reset boots the app instead.
        assert_eq!(
            rig.m.line_state(false, false),
            LineAction::DownloadFlag(false)
        );
        let LineAction::ChipReset { download, .. } = rig.m.line_state(true, false) else {
            panic!("leaving and re-entering (1, 0) resets again");
        };
        assert!(!download);
        // The table is evaluated on every call, so a repeated (0, 1) still sets the flag.
        rig.m.line_state(false, false);
        assert_eq!(
            rig.m.line_state(false, true),
            LineAction::DownloadFlag(true)
        );
        assert_eq!(
            rig.m.line_state(false, true),
            LineAction::DownloadFlag(true)
        );
        assert!(rig.m.download_flag());
    }

    /// The three line sequences esptool and `esp_idf_monitor` send, as `(dtr, rts)` pairs: each
    /// produces exactly one reset, with the download flag that picks the boot mode.
    #[test]
    fn the_esptool_sequences_reach_download_mode_and_then_the_app() {
        let classic_reset_then_flash: &[(bool, bool)] = &[
            (true, false),
            (true, true),
            (false, true),
            (true, true),
            (true, false),
            (false, false),
            (false, true),
            (false, false),
        ];
        let usb_jtag_serial_reset: &[(bool, bool)] = &[
            (true, false),
            (true, true),
            (true, false),
            (false, false),
            (true, false),
            (true, true),
            (false, true),
            (false, false),
        ];
        let idf_monitor_reset: &[(bool, bool)] = &[
            (true, false),
            (true, true),
            (true, false),
            (false, false),
            (false, true),
            (false, false),
        ];
        let resets = |seq: &[(bool, bool)]| {
            let mut m = UsjModel::default();
            let mut out = Vec::new();
            for (dtr, rts) in seq {
                if let LineAction::ChipReset { kind, download } = m.line_state(*rts, *dtr) {
                    assert_eq!(kind.cause, ResetCause::USB_UART_CHIP);
                    out.push(download);
                }
            }
            out
        };
        assert_eq!(
            resets(classic_reset_then_flash),
            vec![true, false],
            "download mode, then the app after the flash",
        );
        assert_eq!(resets(usb_jtag_serial_reset), vec![true], "download mode");
        assert_eq!(resets(idf_monitor_reset), vec![false], "a normal boot");
    }

    #[test]
    fn the_capture_modes_differ_only_while_the_host_is_not_reading() {
        let mut rig = Rig::open();
        rig.link(HostLink::IDLE);
        rig.puts(b"unseen");
        assert!(rig.m.capture().is_empty(), "wire records the wire");

        let mut rig = Rig::open();
        rig.m.set_capture_mode(Capture::Fifo);
        rig.link(HostLink::IDLE);
        rig.puts(b"kept");
        assert_eq!(rig.m.capture(), b"kept", "fifo records the endpoint");

        for mode in [Capture::Wire, Capture::Fifo] {
            let mut rig = Rig::open();
            rig.m.set_capture_mode(mode);
            rig.puts(b"once");
            assert_eq!(rig.m.capture(), b"once", "{mode:?} records each byte once");
        }
    }

    #[test]
    fn a_reset_keeps_the_host_link_and_the_download_flag() {
        let mut rig = Rig::open();
        rig.m.line_state(false, true);
        rig.write(off::INT_ENA, INT_SOF);
        rig.m.push_out_packet(b"y");
        rig.advance(SOF_PERIOD.0);
        rig.puts(b"banner");
        assert_eq!(rig.m.frame_num(), 1);

        let kind = ResetKind::of(ResetCause::USB_UART_CHIP).expect("cause 0x15 is documented");
        rig.m.reset_to(kind, rig.now, &mut rig.sched);
        assert_eq!(rig.read(off::INT_ENA), 0);
        assert_eq!(rig.read(off::CONF0), 0x4200, "the CONF0 reset value");
        assert_eq!(rig.m.frame_num(), 0);
        assert_eq!(rig.m.out_len(), 0);
        assert_ne!(rig.read(off::EP1_CONF) & EP1_CONF_IN_DATA_FREE, 0);
        assert!(
            rig.m.download_flag(),
            "the flag outlives the reset it caused"
        );
        assert_eq!(
            rig.m.link(),
            HostLink::OPEN,
            "the host is not part of the SoC"
        );
        assert!(rig.m.sof_running(), "SOF is re-armed after the reset");
        assert_eq!(
            rig.m.capture(),
            b"banner",
            "a reset never swallows the console"
        );

        // A CPU0_ reset reaches the hart and SENSITIVE only (`ResetFanout::CpuAndPms`).
        rig.write(off::INT_ENA, INT_SOF);
        let cpu = ResetKind::of(ResetCause::RTC_SW_CPU).expect("cause 0x0C is documented");
        rig.m.reset_to(cpu, rig.now, &mut rig.sched);
        assert_eq!(rig.read(off::INT_ENA), INT_SOF);
    }

    /// The download flag is SoC digital state in the `mcu_rail` domain, so a power cycle forgets
    /// it while every other reset keeps it. A flag that survived a power cycle would send the next
    /// (RTS, DTR) = (1, 0) into download mode on a device that should boot its app.
    #[test]
    fn a_power_cycle_clears_the_download_flag_and_every_other_reset_keeps_it() {
        let mut rig = Rig::open();
        assert_eq!(
            rig.m.line_state(false, true),
            LineAction::DownloadFlag(true)
        );
        assert!(rig.m.download_flag());

        rig.power_on();
        assert!(!rig.m.download_flag(), "the rail dropped, so the flag did");
        let LineAction::ChipReset { download, .. } = rig.m.line_state(true, false) else {
            panic!("(RTS, DTR) = (1, 0) resets the chip");
        };
        assert!(!download, "so this boots the app, not download mode");

        // Every other cause keeps it, which the esptool sequence needs.
        rig.m.line_state(false, true);
        for cause in [ResetCause::USB_UART_CHIP, ResetCause::RTC_SW_SYS] {
            let kind = ResetKind::of(cause).expect("a documented cause");
            assert_ne!(kind.scope, ResetScope::Chip);
            rig.m.reset_to(kind, rig.now, &mut rig.sched);
            assert!(rig.m.download_flag(), "cause {:#04x}", cause.0);
        }
    }

    #[test]
    fn source_twenty_six_follows_the_interrupt_level() {
        assert_eq!(SOURCE, IrqSource(26), "ETS_USB_SERIAL_JTAG_INTR_SOURCE");
        let mut rig = Rig::open();
        let mut fabric = crate::intc::IrqFabric::new();
        let start = fabric.epoch();
        rig.m.sync_irq(&mut fabric);
        assert!(!fabric.source(SOURCE), "INT_ENA is 0 after reset");

        rig.advance(SOF_PERIOD.0);
        rig.m.sync_irq(&mut fabric);
        assert!(
            !fabric.source(SOURCE),
            "INT_RAW alone does not drive the source"
        );
        assert_eq!(fabric.epoch(), start, "and the fabric saw no change");

        rig.write(off::INT_ENA, INT_SOF);
        rig.m.sync_irq(&mut fabric);
        assert!(fabric.source(SOURCE), "the fabric holds source 26 high");
        let raised = fabric.epoch();
        rig.m.sync_irq(&mut fabric);
        rig.advance(SOF_PERIOD.0);
        rig.m.sync_irq(&mut fabric);
        assert_eq!(fabric.epoch(), raised, "no second rising edge");

        rig.write(off::INT_CLR, INT_SOF);
        rig.m.sync_irq(&mut fabric);
        assert!(!fabric.source(SOURCE), "and low after INT_CLR");
        let lowered = fabric.epoch();
        rig.m.sync_irq(&mut fabric);
        assert_eq!(fabric.epoch(), lowered, "nothing on the repeat");
    }

    #[test]
    fn turning_the_pads_off_detaches_the_host() {
        let mut rig = Rig::open();
        let conf0 = rig.read(off::CONF0);
        assert_eq!(conf0, CONF0_PAD_ENABLE | CONF0_DP_PULLUP);

        rig.write(off::CONF0, conf0 & !CONF0_PAD_ENABLE);
        assert!(!rig.m.sof_running());
        rig.puts(b"gone");
        assert!(rig.m.capture().is_empty());

        rig.write(off::CONF0, conf0);
        assert!(rig.m.sof_running(), "the host re-enumerates");
        assert_eq!(rig.m.capture(), b"gone", "and takes what was waiting");

        rig.write(
            off::CONF0,
            (conf0 | CONF0_PAD_PULL_OVERRIDE) & !CONF0_DP_PULLUP,
        );
        assert!(!rig.m.sof_running(), "the D+ pull-up is forced off");
    }

    #[test]
    fn the_offsets_match_the_generated_table() {
        for (offset, reg) in [
            (off::EP1, idx::USB_SERIAL_JTAG_EP1),
            (off::EP1_CONF, idx::USB_SERIAL_JTAG_EP1_CONF),
            (off::INT_RAW, idx::USB_SERIAL_JTAG_INT_RAW),
            (off::INT_ST, idx::USB_SERIAL_JTAG_INT_ST),
            (off::INT_ENA, idx::USB_SERIAL_JTAG_INT_ENA),
            (off::INT_CLR, idx::USB_SERIAL_JTAG_INT_CLR),
            (off::CONF0, idx::USB_SERIAL_JTAG_CONF0),
            (off::FRAM_NUM, idx::USB_SERIAL_JTAG_FRAM_NUM),
            (OFF_OUT_EP1_ST, idx::USB_SERIAL_JTAG_OUT_EP1_ST),
        ] {
            assert_eq!(u32::from(REGS[reg].off), offset, "{}", REGS[reg].name);
        }
        assert_eq!(<UsjModel as Peripheral>::ID, id::USJ);
        assert_eq!(<UsjModel as Peripheral>::BASE, 0x6004_3000);
        assert_eq!(<UsjModel as Peripheral>::SIZE, BLOCK_SIZE);
        assert_eq!(SOURCE, IrqSource(26), "ETS_USB_SERIAL_JTAG_INTR_SOURCE");
    }

    #[test]
    fn the_fidelity_and_stability_answers_come_from_the_spec_rows() {
        let mut rig = Rig::open();
        assert_eq!(rig.m.fidelity(off::EP1), Fidelity::A);
        assert_eq!(rig.m.fidelity(off::EP1_CONF), Fidelity::A);
        assert_eq!(rig.m.fidelity(off::INT_RAW), Fidelity::B);
        assert_eq!(rig.m.fidelity(off::FRAM_NUM), Fidelity::B);
        assert_eq!(rig.m.fidelity(0x0FFC), Fidelity::U, "no register there");

        assert!(matches!(
            rig.m.stability(off::FRAM_NUM),
            Stability::UntilNextEvent
        ));
        assert!(matches!(
            rig.m.stability(off::INT_RAW),
            Stability::UntilNextEvent
        ));
        assert!(matches!(
            rig.m.stability(off::EP1_CONF),
            Stability::UntilInput
        ));
        rig.link(HostLink::DETACHED);
        assert!(
            matches!(rig.m.stability(off::FRAM_NUM), Stability::UntilInput),
            "with SOF stopped only host input moves FRAME_NUM",
        );

        assert_eq!(rig.read(0x0FFC), 0);
        rig.write(0x0FFC, 0xFFFF_FFFF);
        assert_eq!(rig.read(0x0FFC), 0);
    }

    #[test]
    fn first_touches_reach_the_ledger_once() {
        let mut rig = Rig::open();
        rig.read(off::EP1_CONF);
        rig.read(off::EP1_CONF);
        rig.write(off::INT_ENA, 0);
        let touches: Vec<_> = rig
            .ledger
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access))
            .collect();
        assert_eq!(
            touches,
            vec![
                (off::EP1_CONF, TouchAccess::Read),
                (off::INT_ENA, TouchAccess::Write),
            ],
        );
        for t in rig.ledger.first_touches() {
            assert_eq!(t.periph, id::USJ);
            assert!(!t.allowlisted);
        }
    }
}
