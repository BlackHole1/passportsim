//! I2C0 master and its command-list executor (`specs/blocks/i2c0.toml`).
//!
//! The driver loads `COMD0` to `COMD7` with `RESTART`, `WRITE`, `READ`, `STOP` and `END` steps,
//! pushes the bytes into the TX FIFO through `I2C_DATA` and writes `CTR.trans_start`. That write
//! runs the whole list against the bus inside the access (`Wiring::I2cRun`, `wiring::i2c`), so the
//! driver's first poll of `INT_RAW` sees `TRANS_COMPLETE` or `NACK`.
//!
//! An address no chip claims must NACK at once and stop the list: the boot scan probes 112
//! addresses and only 0x18 and 0x63 answer, and a timeout would cost 50 ms plus a bus reset each.
//!
//! Under `fast` the list takes zero virtual time and schedules nothing (the driver works either
//! way). Under `i2c_clocked` the bus work still happens in the access, but the bits the driver
//! waits on and the release of `SR.bus_busy` are held back ([`Model::run_timed`]) and delivered by
//! an event at the list's bus time. The event tag carries what it delivers ([`Deferred::tag`]), so
//! nothing new is serialized.
//!
//! `INT_STATUS` is the level of source 29. The list runs without a `Cx`, so the machine calls
//! [`Model::sync_irq`] right after it.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::PeriphId;
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use crate::r#gen::regs_i2c0::{BLOCK_SIZE, REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};
use crate::intc::IrqFabric;

pub const IRQ_SOURCE: IrqSource = irq::I2C_EXT0;

pub const FIFO_DEPTH: usize = 32;

pub const COMD_COUNT: usize = 8;

/// `CTR.trans_start`, bit 5, self-clearing.
const CTR_TRANS_START: u32 = 1 << 5;
/// `CTR.fsm_rst`, bit 10, self-clearing.
const CTR_FSM_RST: u32 = 1 << 10;
/// `CTR.conf_upgate`, bit 11, self-clearing.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "register map; only the tests write it")
)]
const CTR_CONF_UPGATE: u32 = 1 << 11;

/// `FIFO_CONF.rx_fifo_rst`, bit 12; pulsed 1 then 0 to empty the RX FIFO.
const FIFO_CONF_RX_RST: u32 = 1 << 12;
const FIFO_CONF_TX_RST: u32 = 1 << 13;

/// `SR.resp_rec`, bit 0: the last acknowledgement, 1 meaning NACK.
const SR_RESP_REC: u32 = 1 << 0;
/// `SR.bus_busy`, bit 4: 1 from START until STOP.
const SR_BUS_BUSY: u32 = 1 << 4;

/// Marks an event tag as a deferred list completion ([`Deferred::tag`]).
const TAG_DONE: u16 = 0x8000;
/// In a [`TAG_DONE`] tag: the completion releases `SR.bus_busy`.
const TAG_RELEASE: u16 = 0x4000;
/// In a [`TAG_DONE`] tag: the `INT_RAW` bits it raises (all of them below bit 12).
const TAG_BITS: u16 = 0x0FFF;

/// Picoseconds per cycle of the I2C source clock: the 40 MHz XTAL with `SCLK_SEL` 0, and RC_FAST
/// with `SCLK_SEL` 1 at the measured rate of [`super::timg::RC_FAST_HZ`].
const XTAL_PS: u64 = 25_000;
const RC_FAST_PS: u64 = (1_000_000_000_000 + super::timg::RC_FAST_HZ / 2) / super::timg::RC_FAST_HZ;
/// Shift of `SR.rxfifo_cnt`, bits [13:8].
const SR_RXFIFO_CNT_SHIFT: u32 = 8;
/// Shift of `SR.txfifo_cnt`, bits [23:18].
const SR_TXFIFO_CNT_SHIFT: u32 = 18;
/// Mask of a FIFO counter field, six bits wide.
const SR_FIFO_CNT_MASK: u32 = 0x3F;

pub const INT_RXFIFO_OVF: u32 = 1 << 2;
pub const INT_END_DETECT: u32 = 1 << 3;
pub const INT_TRANS_COMPLETE: u32 = 1 << 7;
pub const INT_NACK: u32 = 1 << 10;
pub const INT_TXFIFO_OVF: u32 = 1 << 11;
pub const INT_RXFIFO_UDF: u32 = 1 << 12;

const COMD_DONE: u32 = 1 << 31;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Op {
    /// Op code 1: clock `byte_num` bytes out of the TX FIFO.
    Write,
    /// Op code 2: stop condition; the list ends here.
    Stop,
    /// Op code 3: clock `byte_num` bytes into the RX FIFO.
    Read,
    /// Op code 4: raise `END_DETECT` and pause the list.
    End,
    /// Op code 6: start or repeated start; the next written byte is an address byte.
    Restart,
    /// Anything else. The C3 defines no other op code, so the list stops.
    Unknown(u8),
}

/// One decoded `COMDn` register: `byte_num` [7:0], `ack_en` 8, `ack_exp` 9, `ack_val` 10,
/// `op_code` [13:11], `command_done` 31.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Cmd {
    pub byte_num: u8,
    /// Check the acknowledgement of every byte written.
    pub ack_en: bool,
    /// Acknowledgement the master sends after the last byte of a `Read` step.
    pub ack_val: bool,
    pub op: Op,
}

impl Cmd {
    /// The `COMDn` value of this step, the inverse of [`Cmd::decode`]; `ack_exp` stays 0, the only
    /// value the driver writes.
    pub const fn encode(self) -> u32 {
        let op = match self.op {
            Op::Write => 1,
            Op::Stop => 2,
            Op::Read => 3,
            Op::End => 4,
            Op::Restart => 6,
            Op::Unknown(code) => code as u32,
        };
        (self.byte_num as u32)
            | ((self.ack_en as u32) << 8)
            | ((self.ack_val as u32) << 10)
            | (op << 11)
    }

    pub const fn decode(val: u32) -> Cmd {
        Cmd {
            byte_num: (val & 0xFF) as u8,
            ack_en: val & (1 << 8) != 0,
            ack_val: val & (1 << 10) != 0,
            op: match (val >> 11) & 0x7 {
                1 => Op::Write,
                2 => Op::Stop,
                3 => Op::Read,
                4 => Op::End,
                6 => Op::Restart,
                other => Op::Unknown(other as u8),
            },
        }
    }
}

/// The four bus operations a command list performs, in wire order. `wiring::i2c` implements it
/// over `BoardPorts`, so the block never reaches the board directly.
pub trait I2cBus {
    /// Start or repeated start to a 7-bit address; returns the acknowledgement. An address no chip
    /// claims must answer `false`.
    fn start(&mut self, addr: u8, read: bool) -> bool;
    /// One byte written to the addressed chip; returns the acknowledgement.
    fn write(&mut self, byte: u8) -> bool;
    fn read(&mut self) -> u8;
    /// Stop condition; the bus is released.
    fn stop(&mut self);
}

/// How a command-list run ended.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RunEnd {
    /// A `Stop` step ran: `TRANS_COMPLETE` is raised and the bus is released.
    Complete,
    /// A byte was not acknowledged while `ack_en` was set: `NACK` is raised, the bus is released
    /// and the rest of the list is skipped.
    Nack,
    /// An `End` step ran: `END_DETECT` is raised and the bus stays busy.
    EndDetect,
    /// The eight command registers ran out, or a step carried no op code the C3 defines.
    Exhausted,
}

/// A 32-byte FIFO. Overflow and underflow are reported through `INT_RAW`, so a driver bug shows
/// as the bit the hardware would raise.
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Fifo {
    bytes: Vec<u8>,
}

impl Fifo {
    fn push(&mut self, byte: u8) -> bool {
        if self.bytes.len() >= FIFO_DEPTH {
            return false;
        }
        self.bytes.push(byte);
        true
    }

    fn pop(&mut self) -> Option<u8> {
        (!self.bytes.is_empty()).then(|| self.bytes.remove(0))
    }

    fn len(&self) -> u32 {
        self.bytes.len() as u32
    }

    fn clear(&mut self) {
        self.bytes.clear();
    }
}

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    tx: Fifo,
    rx: Fifo,
    /// Bit `i` set once register `i` has been reported to the ledger. Part of the snapshot, so a
    /// restored machine does not report a register twice.
    touched: u64,
}

crate::regs::store_serde!();

impl Default for Model {
    fn default() -> Self {
        let mut model = Model {
            regs: RegStore::new(&REGS),
            tx: Fifo::default(),
            rx: Fifo::default(),
            touched: 0,
        };
        model.refresh_status();
        model
    }
}

impl Model {
    /// Recomputes the `SR` FIFO counters and `INT_STATUS` (`INT_RAW & INT_ENA`).
    fn refresh_status(&mut self) {
        let sr = self.regs.get(idx::I2C_SR)
            & !(SR_FIFO_CNT_MASK << SR_RXFIFO_CNT_SHIFT)
            & !(SR_FIFO_CNT_MASK << SR_TXFIFO_CNT_SHIFT);
        self.regs.set(
            idx::I2C_SR,
            sr | (self.rx.len() & SR_FIFO_CNT_MASK) << SR_RXFIFO_CNT_SHIFT
                | (self.tx.len() & SR_FIFO_CNT_MASK) << SR_TXFIFO_CNT_SHIFT,
        );
        let st = self.regs.get(idx::I2C_INT_RAW) & self.regs.get(idx::I2C_INT_ENA);
        self.regs.set(idx::I2C_INT_STATUS, st);
    }

    fn raise(&mut self, bits: u32) {
        let raw = self.regs.get(idx::I2C_INT_RAW) | bits;
        self.regs.set(idx::I2C_INT_RAW, raw);
    }

    fn set_sr(&mut self, bits: u32, on: bool) {
        let sr = self.regs.get(idx::I2C_SR);
        self.regs
            .set(idx::I2C_SR, if on { sr | bits } else { sr & !bits });
    }

    /// The decoded command list, `COMD0` to `COMD7` in order.
    pub fn commands(&self) -> [Cmd; COMD_COUNT] {
        core::array::from_fn(|i| Cmd::decode(self.regs.get(idx::I2C_COMD0 + i)))
    }

    /// Until when a read of `off` keeps returning the same value, for the hang detector. Only an
    /// access or the block's own completion event changes `INT_RAW` and `SR`; under `fast` no
    /// event is scheduled, so a poll that never writes is a real hang.
    pub fn stability(&self, off: u32) -> Stability {
        match regs::reg_at(&REGS, off) {
            Some((idx, _)) if REGS[idx].stable_read => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Stored value of the register at block offset `off`, without side effects or reporting.
    pub fn reg(&self, off: u32) -> u32 {
        regs::reg_at(&REGS, off).map_or(0, |(idx, _)| self.regs.get(idx))
    }

    pub fn tx_bytes(&self) -> &[u8] {
        &self.tx.bytes
    }

    pub fn rx_bytes(&self) -> &[u8] {
        &self.rx.bytes
    }

    /// Restores the registers and empties both FIFOs on every reset that reaches the blocks.
    pub fn reset_regs(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.tx.clear();
        self.rx.clear();
        self.refresh_status();
    }

    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::I2C_INT_STATUS) != 0
    }

    /// Drives [`IRQ_SOURCE`] to [`Model::irq_level`]. The machine also calls it after
    /// `wiring::i2c::run`, which raises `INT_RAW` outside any entry point.
    pub fn sync_irq(&self, irq: &mut IrqFabric) {
        irq.set_source(IRQ_SOURCE, self.irq_level());
    }

    pub fn bus_busy(&self) -> bool {
        self.regs.get(idx::I2C_SR) & SR_BUS_BUSY != 0
    }
}

/// What a clocked list delivers at its end rather than inside the access ([`Model::run_timed`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct Deferred {
    /// `INT_RAW` bits the end of the list raises: `TRANS_COMPLETE`, `NACK` or `END_DETECT`.
    pub bits: u32,
    /// Whether the end of the list releases `SR.bus_busy` (a `STOP`, or the stop after a NACK).
    pub release: bool,
    /// SCL periods the list occupied: nine per byte, one per `RESTART` and one per `STOP`. Setup
    /// and hold times are inside a period (class C).
    pub periods: u64,
}

impl Deferred {
    /// The event tag that delivers this completion: [`TAG_DONE`], [`TAG_RELEASE`] and the bits.
    pub fn tag(&self) -> u16 {
        TAG_DONE | if self.release { TAG_RELEASE } else { 0 } | (self.bits as u16 & TAG_BITS)
    }
}

impl Model {
    /// Picoseconds per SCL period: `SCL_LOW_PERIOD + 1` low and `SCL_HIGH_PERIOD +
    /// SCL_WAIT_HIGH_PERIOD` high source-clock cycles, divided by `SCLK_DIV_NUM + 1` (IDF writes
    /// 199, 100 and 100 for 100 kHz from 40 MHz). The fractional divider and rise time are left
    /// out (class C).
    pub fn scl_period_ps(&self) -> u64 {
        let low = u64::from(self.regs.get(idx::I2C_SCL_LOW_PERIOD) & 0x1FF) + 1;
        let high_reg = self.regs.get(idx::I2C_SCL_HIGH_PERIOD);
        let high = u64::from(high_reg & 0x1FF) + u64::from(high_reg >> 9 & 0x7F);
        let conf = self.regs.get(idx::I2C_CLK_CONF);
        let src = if conf >> 20 & 1 == 1 {
            RC_FAST_PS
        } else {
            XTAL_PS
        };
        let div = u64::from(conf & 0xFF) + 1;
        (low + high) * src * div
    }

    /// The end of a clocked list came due: raises the held-back bits and releases the bus as the
    /// tag says. Any other tag changes nothing.
    pub fn complete_deferred(&mut self, tag: u16) {
        if tag & TAG_DONE == 0 {
            return;
        }
        if tag & TAG_RELEASE != 0 {
            self.set_sr(SR_BUS_BUSY, false);
        }
        self.raise(u32::from(tag & TAG_BITS));
        self.refresh_status();
    }

    /// Runs the loaded command list against `bus`. The first byte a `Write` clocks after a
    /// `Restart` is the address byte; a byte not acknowledged while `ack_en` is set ends the list
    /// with a stop and `NACK`.
    pub fn run(&mut self, bus: &mut dyn I2cBus) -> RunEnd {
        self.run_timed(bus, false).0
    }

    /// [`Model::run`], and with `defer` the bits the end would raise and the bus release come back
    /// as a [`Deferred`] to schedule at the list's bus time. The bus bytes, FIFOs,
    /// `command_done` and `resp_rec` happen in the access either way.
    pub fn run_timed(&mut self, bus: &mut dyn I2cBus, defer: bool) -> (RunEnd, Deferred) {
        let mut held = Deferred::default();
        let end = self.run_list(bus, &mut held);
        if !defer {
            if held.release {
                self.set_sr(SR_BUS_BUSY, false);
            }
            self.raise(held.bits);
            self.refresh_status();
        }
        (end, held)
    }

    fn run_list(&mut self, bus: &mut dyn I2cBus, held: &mut Deferred) -> RunEnd {
        // Raised for the whole list rather than at the first start: nothing reads it mid-list,
        // so the two placements are indistinguishable to the driver.
        self.set_sr(SR_BUS_BUSY, true);
        let mut addressing = false;
        let commands = self.commands();
        for (i, cmd) in commands.iter().enumerate() {
            let end = match cmd.op {
                Op::Restart => {
                    addressing = true;
                    held.periods += 1;
                    None
                }
                Op::Write => self.step_write(*cmd, &mut addressing, bus, held),
                Op::Read => {
                    self.step_read(*cmd, bus);
                    held.periods += 9 * u64::from(cmd.byte_num);
                    None
                }
                Op::Stop => {
                    bus.stop();
                    held.periods += 1;
                    held.release = true;
                    held.bits |= INT_TRANS_COMPLETE;
                    Some(RunEnd::Complete)
                }
                Op::End => {
                    held.bits |= INT_END_DETECT;
                    Some(RunEnd::EndDetect)
                }
                Op::Unknown(_) => Some(RunEnd::Exhausted),
            };
            if !matches!(cmd.op, Op::Unknown(_)) {
                self.mark_done(i);
            }
            if let Some(end) = end {
                self.refresh_status();
                return end;
            }
        }
        self.refresh_status();
        RunEnd::Exhausted
    }

    /// One `Write` step. Returns `Some(RunEnd::Nack)` when the list must stop.
    fn step_write(
        &mut self,
        cmd: Cmd,
        addressing: &mut bool,
        bus: &mut dyn I2cBus,
        held: &mut Deferred,
    ) -> Option<RunEnd> {
        for _ in 0..cmd.byte_num {
            held.periods += 9;
            // An empty TX FIFO underruns rather than stalling; the byte on the wire is
            // UNVERIFIED.
            let byte = self.tx.pop().unwrap_or(0);
            let ack = if *addressing {
                *addressing = false;
                bus.start(byte >> 1, byte & 1 == 1)
            } else {
                bus.write(byte)
            };
            self.set_sr(SR_RESP_REC, !ack);
            if cmd.ack_en && !ack {
                bus.stop();
                held.periods += 1;
                held.release = true;
                held.bits |= INT_NACK;
                return Some(RunEnd::Nack);
            }
        }
        None
    }

    /// One `Read` step: `byte_num` bytes into the RX FIFO. The final `ack_val` is decoded but not
    /// replayed, since no chip model observes it.
    fn step_read(&mut self, cmd: Cmd, bus: &mut dyn I2cBus) {
        for _ in 0..cmd.byte_num {
            let byte = bus.read();
            if !self.rx.push(byte) {
                self.raise(INT_RXFIFO_OVF);
            }
        }
    }

    fn mark_done(&mut self, i: usize) {
        let reg = idx::I2C_COMD0 + i;
        let val = self.regs.get(reg) | COMD_DONE;
        self.regs.set(reg, val);
    }
}

impl Model {
    /// Reads `size` bytes at block offset `off`, reporting first touches. `DATA` pops the RX FIFO.
    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((idx, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, idx, TouchAccess::Read, at, ledger);
        let val = if idx == idx::I2C_DATA {
            match self.rx.pop() {
                Some(byte) => u32::from(byte),
                None => {
                    self.raise(INT_RXFIFO_UDF);
                    0
                }
            }
        } else {
            self.regs.read(idx, byte_off, size)
        };
        self.refresh_status();
        val
    }

    /// Writes the low `size` bytes of `val` at block offset `off`. Returns `Wiring::I2cRun` for a
    /// `CTR.trans_start` write.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        ledger: &mut FidelityLedger,
    ) -> Wiring {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((idx, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Write, at, ledger);
            return Wiring::None;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, idx, TouchAccess::Write, at, ledger);
        if idx == idx::I2C_DATA {
            if byte_off == 0 && !self.tx.push((val & 0xFF) as u8) {
                self.raise(INT_TXFIFO_OVF);
            }
            self.refresh_status();
            return Wiring::None;
        }
        let delta = self.regs.write(idx, byte_off, size, val);
        let mut wiring = Wiring::None;
        match idx {
            idx::I2C_CTR => {
                if delta.triggers & CTR_FSM_RST != 0 {
                    // `i2c_ll_master_fsm_rst`: the FSM goes idle and releases the bus. The FIFOs
                    // are reset separately through `FIFO_CONF`.
                    self.set_sr(SR_BUS_BUSY, false);
                }
                // `conf_upgate` needs no effect: values apply as written, and the bit already
                // reads back 0, which is all the driver polls for.
                if delta.triggers & CTR_TRANS_START != 0 {
                    wiring = Wiring::I2cRun;
                }
            }
            idx::I2C_INT_CLR => {
                let raw = self.regs.get(idx::I2C_INT_RAW) & !delta.triggers;
                self.regs.set(idx::I2C_INT_RAW, raw);
            }
            idx::I2C_FIFO_CONF => {
                let rising = delta.after & !delta.before;
                if rising & FIFO_CONF_RX_RST != 0 {
                    self.rx.clear();
                }
                if rising & FIFO_CONF_TX_RST != 0 {
                    self.tx.clear();
                }
            }
            idx::I2C_SCL_SP_CONF => {
                // `scl_rst_slv_en` is polled until 0; the bus-clear pulses take no time here.
                self.regs.clear_sc(idx, delta.triggers);
            }
            _ => {}
        }
        self.refresh_status();
        wiring
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <block::I2c0 as Block>::ID;
    const BASE: u32 = <block::I2c0 as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_regs(kind);
        self.sync_irq(cx.irq);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.store(off, size, val, cx.now, cx.ledger);
        self.sync_irq(cx.irq);
        RegWrite {
            stop: !matches!(wiring, Wiring::None),
            wiring,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        self.complete_deferred(tag);
        self.sync_irq(cx.irq);
        Wiring::None
    }

    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        self.stability(off)
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(idx, _)| REGS[idx].class)
    }
}

#[cfg(test)]
mod tests {
    use pemu_core::fidelity::LedgerSubject;
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};

    use super::*;

    const CTR: u32 = 0x004;
    const SR: u32 = 0x008;
    const FIFO_CONF: u32 = 0x018;
    const DATA: u32 = 0x01C;
    const INT_RAW: u32 = 0x020;
    const INT_CLR: u32 = 0x024;
    const INT_ENA: u32 = 0x028;
    const INT_STATUS: u32 = 0x02C;
    const COMD0: u32 = 0x058;
    const SCL_SP_CONF: u32 = 0x080;

    const T: VTime = VTime(1_000);

    /// Under `fast` this block schedules no event, so a poll that never writes is a real hang
    /// and the detector may say so.
    #[test]
    fn the_busy_wait_row_answers_the_hang_detector_with_a_stable_read() {
        let model = Model::default();
        for off in [INT_RAW, SR] {
            assert!(
                matches!(model.stability(off), Stability::UntilNextEvent),
                "{off:#05X} is a stable_read row",
            );
        }
        assert!(matches!(model.stability(CTR), Stability::Never));
        assert!(
            matches!(model.stability(0x0F0), Stability::Never),
            "an offset with no register row",
        );
    }

    #[derive(Default)]
    struct StubBus {
        present: Vec<u8>,
        reads: Vec<u8>,
        read_cursor: usize,
        addressed: Option<u8>,
        log: Vec<String>,
    }

    impl StubBus {
        fn with(present: &[u8], reads: &[u8]) -> StubBus {
            StubBus {
                present: present.to_vec(),
                reads: reads.to_vec(),
                ..StubBus::default()
            }
        }
    }

    impl I2cBus for StubBus {
        fn start(&mut self, addr: u8, read: bool) -> bool {
            let ack = self.present.contains(&addr);
            self.addressed = ack.then_some(addr);
            self.log
                .push(format!("S {addr:02X} {} {}", u8::from(read), u8::from(ack)));
            ack
        }

        fn write(&mut self, byte: u8) -> bool {
            let ack = self.addressed.is_some();
            self.log.push(format!("W {byte:02X} {}", u8::from(ack)));
            ack
        }

        fn read(&mut self) -> u8 {
            let byte = self.reads.get(self.read_cursor).copied().unwrap_or(0xFF);
            self.read_cursor += 1;
            self.log.push(format!("R {byte:02X}"));
            byte
        }

        fn stop(&mut self) {
            self.addressed = None;
            self.log.push("P".to_string());
        }
    }

    /// Loads a command list and its TX bytes, then starts it, as the driver does.
    fn start(model: &mut Model, ledger: &mut FidelityLedger, cmds: &[Cmd], tx: &[u8]) -> Wiring {
        for (i, cmd) in cmds.iter().enumerate() {
            model.store(COMD0 + 4 * i as u32, Size::B4, cmd.encode(), T, ledger);
        }
        for byte in tx {
            model.store(DATA, Size::B4, u32::from(*byte), T, ledger);
        }
        model.store(CTR, Size::B4, CTR_CONF_UPGATE | CTR_TRANS_START, T, ledger)
    }

    fn cmd(op: Op, byte_num: u8, ack_en: bool) -> Cmd {
        Cmd {
            byte_num,
            ack_en,
            ack_val: false,
            op,
        }
    }

    /// The probe list: `RESTART; WRITE byte_num 1 ack_en; STOP` with `addr<<1`.
    fn probe_list() -> [Cmd; 3] {
        [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 1, true),
            cmd(Op::Stop, 0, false),
        ]
    }

    #[test]
    fn the_model_is_the_i2c0_row_of_the_table() {
        assert_eq!(<Model as Peripheral>::ID, super::super::id::I2C0);
        assert_eq!(<Model as Peripheral>::BASE, 0x6001_3000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
        let model = Model::default();
        assert_eq!(model.fidelity(INT_RAW), Fidelity::B);
        assert_eq!(model.fidelity(SR), Fidelity::B);
    }

    /// Op codes: WRITE 1, STOP 2, READ 3, END 4, RESTART 6.
    #[test]
    fn command_registers_decode_and_encode_the_c3_op_codes() {
        for (op, code) in [
            (Op::Write, 1),
            (Op::Stop, 2),
            (Op::Read, 3),
            (Op::End, 4),
            (Op::Restart, 6),
        ] {
            let c = Cmd {
                byte_num: 3,
                ack_en: true,
                ack_val: true,
                op,
            };
            let encoded = c.encode();
            assert_eq!((encoded >> 11) & 0x7, code, "{op:?}");
            assert_eq!(encoded & 0xFF, 3);
            assert_eq!(encoded & (1 << 8), 1 << 8);
            assert_eq!(encoded & (1 << 9), 0, "ack_exp stays 0");
            assert_eq!(encoded & (1 << 10), 1 << 10);
            assert_eq!(Cmd::decode(encoded), c);
        }
        assert_eq!(Cmd::decode(0).op, Op::Unknown(0));
        assert_eq!(Cmd::decode(5 << 11).op, Op::Unknown(5));
    }

    /// An address no chip claims raises `NACK` inside the `trans_start` access, stops the list
    /// and releases the bus.
    #[test]
    fn a_probe_of_an_absent_address_nacks_inside_the_starting_access() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x18, 0x63], &[]);
        let wiring = start(&mut model, &mut ledger, &probe_list(), &[0x20 << 1]);
        assert!(matches!(wiring, Wiring::I2cRun));
        assert_eq!(model.run(&mut bus), RunEnd::Nack);

        let raw = model.load(INT_RAW, Size::B4, T, &mut ledger);
        assert_eq!(raw & INT_NACK, INT_NACK, "NACK is raised");
        assert_eq!(raw & INT_TRANS_COMPLETE, 0, "the list did not reach STOP");
        assert!(!model.bus_busy(), "the NACK path releases the bus");
        let sr = model.load(SR, Size::B4, T, &mut ledger);
        assert_eq!(sr & SR_RESP_REC, SR_RESP_REC, "resp_rec records the NACK");
        assert_eq!(bus.log, ["S 20 0 0", "P"]);
    }

    /// An address a chip claims runs the list to its `STOP` and raises `TRANS_COMPLETE`.
    #[test]
    fn a_probe_of_a_present_address_completes_the_list() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x18, 0x63], &[]);
        start(&mut model, &mut ledger, &probe_list(), &[0x18 << 1]);
        assert_eq!(model.run(&mut bus), RunEnd::Complete);

        let raw = model.load(INT_RAW, Size::B4, T, &mut ledger);
        assert_eq!(raw & INT_TRANS_COMPLETE, INT_TRANS_COMPLETE);
        assert_eq!(raw & INT_NACK, 0);
        assert!(!model.bus_busy(), "STOP released the bus");
        assert_eq!(model.load(SR, Size::B4, T, &mut ledger) & SR_RESP_REC, 0);
        assert_eq!(bus.log, ["S 18 0 1", "P"]);
        for i in 0..probe_list().len() as u32 {
            assert_eq!(
                model.reg(COMD0 + 4 * i) & COMD_DONE,
                COMD_DONE,
                "COMD{i} done"
            );
        }
    }

    /// `SCLK_SEL` 1 clocks SCL from RC_FAST at the measured `timg::RC_FAST_HZ`, not the nominal
    /// 17.5 MHz: 400 cycles take 22.554 us, where the nominal rate gave 22.857.
    #[test]
    fn the_rc_fast_source_runs_at_the_measured_rate() {
        const SCL_LOW: u32 = 0x000;
        const SCL_HIGH: u32 = 0x038;
        const CLK_CONF: u32 = 0x054;
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(SCL_LOW, Size::B4, 199, T, &mut ledger);
        model.store(SCL_HIGH, Size::B4, 100 | (100 << 9), T, &mut ledger);
        model.store(CLK_CONF, Size::B4, 1 << 20, T, &mut ledger);
        assert_eq!(RC_FAST_PS, 56_384);
        assert_eq!(model.scl_period_ps(), 400 * 56_384);
    }

    /// The bus time is 9 periods per byte plus one per RESTART and STOP, at the 10 us period IDF
    /// programs for 100 kHz.
    #[test]
    fn a_clocked_list_delivers_its_end_by_event_at_its_bus_time() {
        const SCL_LOW: u32 = 0x000;
        const SCL_HIGH: u32 = 0x038;
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(SCL_LOW, Size::B4, 199, T, &mut ledger);
        model.store(SCL_HIGH, Size::B4, 100 | (100 << 9), T, &mut ledger);
        assert_eq!(model.scl_period_ps(), 10_000_000, "400 XTAL cycles, 10 us");

        let mut bus = StubBus::with(&[0x18], &[]);
        start(&mut model, &mut ledger, &probe_list(), &[0x18 << 1]);
        let (end, held) = model.run_timed(&mut bus, true);
        assert_eq!(end, RunEnd::Complete);
        assert_eq!(
            bus.log,
            ["S 18 0 1", "P"],
            "the bytes went out in the access"
        );
        assert_eq!(held.periods, 1 + 9 + 1);
        assert_eq!(held.bits, INT_TRANS_COMPLETE);
        assert!(held.release);
        let ends = INT_TRANS_COMPLETE | INT_NACK | INT_END_DETECT;
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & ends,
            0,
            "held back"
        );
        assert!(model.bus_busy(), "the bus is held until the end");

        model.complete_deferred(held.tag());
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & ends,
            INT_TRANS_COMPLETE
        );
        assert!(!model.bus_busy());

        let mut model = Model::default();
        start(&mut model, &mut ledger, &probe_list(), &[0x20 << 1]);
        let (end, held) = model.run_timed(&mut bus, true);
        assert_eq!(end, RunEnd::Nack);
        assert_eq!((held.bits, held.periods), (INT_NACK, 1 + 9 + 1));
        model.complete_deferred(held.tag() & !TAG_DONE);
        assert_eq!(model.load(INT_RAW, Size::B4, T, &mut ledger) & ends, 0);
        model.complete_deferred(held.tag());
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & ends,
            INT_NACK
        );
    }

    /// Register write: `RESTART; WRITE byte_num 3 ack_en; STOP` with `addr<<1, reg, val`.
    #[test]
    fn the_write_command_list_clocks_out_the_address_register_and_value() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x18], &[]);
        let list = [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 3, true),
            cmd(Op::Stop, 0, false),
        ];
        start(&mut model, &mut ledger, &list, &[0x18 << 1, 0x32, 0xB2]);
        assert_eq!(model.run(&mut bus), RunEnd::Complete);
        assert_eq!(bus.log, ["S 18 0 1", "W 32 1", "W B2 1", "P"]);
        assert!(model.tx_bytes().is_empty(), "the TX FIFO drained");
    }

    /// Register read: `RESTART; WRITE 2; RESTART; WRITE 1; READ n-1; READ 1; STOP`. The second
    /// restart turns the direction around.
    #[test]
    fn the_read_command_list_restarts_and_fills_the_rx_fifo() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x63], &[0x0F, 0x94, 0x64]);
        let list = [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 2, true),
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 1, true),
            cmd(Op::Read, 2, false),
            cmd(Op::Read, 1, false),
            cmd(Op::Stop, 0, false),
        ];
        start(
            &mut model,
            &mut ledger,
            &list,
            &[0x63 << 1, 0x00, 0x63 << 1 | 1],
        );
        assert_eq!(model.run(&mut bus), RunEnd::Complete);
        assert_eq!(
            bus.log,
            [
                "S 63 0 1", "W 00 1", "S 63 1 1", "R 0F", "R 94", "R 64", "P"
            ]
        );
        assert_eq!(model.rx_bytes(), [0x0F, 0x94, 0x64]);
        let cnt =
            (model.load(SR, Size::B4, T, &mut ledger) >> SR_RXFIFO_CNT_SHIFT) & SR_FIFO_CNT_MASK;
        assert_eq!(cnt, 3);
        for want in [0x0F, 0x94, 0x64] {
            assert_eq!(model.load(DATA, Size::B4, T, &mut ledger), want);
        }
        assert_eq!(
            model.load(SR, Size::B4, T, &mut ledger) >> SR_RXFIFO_CNT_SHIFT & SR_FIFO_CNT_MASK,
            0
        );
        // A pop past the end underruns rather than stalling.
        assert_eq!(model.load(DATA, Size::B4, T, &mut ledger), 0);
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_RXFIFO_UDF,
            INT_RXFIFO_UDF
        );
    }

    /// An `END` step raises `END_DETECT` and leaves the bus held. No BSP path uses it.
    #[test]
    fn an_end_step_raises_end_detect_and_keeps_the_bus() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x18], &[]);
        let list = [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 1, true),
            cmd(Op::End, 0, false),
        ];
        start(&mut model, &mut ledger, &list, &[0x18 << 1]);
        assert_eq!(model.run(&mut bus), RunEnd::EndDetect);
        assert_eq!(
            model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_END_DETECT,
            INT_END_DETECT
        );
        assert!(model.bus_busy(), "END does not release the bus");
    }

    /// `fsm_rst` also releases the bus.
    #[test]
    fn the_ctr_trigger_bits_read_back_zero_and_fsm_rst_releases_the_bus() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x18], &[]);
        let list = [cmd(Op::Restart, 0, false), cmd(Op::Write, 1, true)];
        start(&mut model, &mut ledger, &list, &[0x18 << 1]);
        assert_eq!(model.run(&mut bus), RunEnd::Exhausted);
        assert!(model.bus_busy(), "a list without STOP leaves the bus held");

        let ctr = model.load(CTR, Size::B4, T, &mut ledger);
        assert_eq!(ctr & CTR_TRANS_START, 0, "trans_start self clears");
        assert_eq!(ctr & CTR_CONF_UPGATE, 0, "conf_upgate self clears");
        assert_eq!(ctr & CTR_FSM_RST, 0, "fsm_rst self clears");

        model.store(CTR, Size::B4, CTR_FSM_RST, T, &mut ledger);
        assert_eq!(model.load(CTR, Size::B4, T, &mut ledger) & CTR_FSM_RST, 0);
        assert!(!model.bus_busy(), "fsm_rst puts the state machine idle");
    }

    #[test]
    fn int_status_drives_source_29() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[], &[]);
        p.with(|cx| Peripheral::write(&mut model, INT_ENA, Size::B4, INT_NACK, cx));
        assert!(!p.source(IRQ_SOURCE), "nothing raised yet");
        start(&mut model, &mut ledger, &probe_list(), &[0x20 << 1]);
        model.run(&mut bus);
        model.sync_irq(&mut p.irq);
        assert!(p.source(IRQ_SOURCE), "the NACK asserts source 29");
        p.with(|cx| Peripheral::write(&mut model, INT_CLR, Size::B4, INT_NACK, cx));
        assert!(!p.source(IRQ_SOURCE), "INT_CLR lowers it");

        start(&mut model, &mut ledger, &probe_list(), &[0x20 << 1]);
        model.run(&mut bus);
        model.sync_irq(&mut p.irq);
        assert!(p.source(IRQ_SOURCE));
        let kind = ResetKind::of(ResetCause::RTC_SW_SYS).expect("a documented reset cause");
        p.with(|cx| Peripheral::reset(&mut model, kind, cx));
        assert!(!p.source(IRQ_SOURCE), "a system reset lowers it");
    }

    #[test]
    fn int_status_masks_int_raw_and_int_clr_clears_it() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[], &[]);
        start(&mut model, &mut ledger, &probe_list(), &[0x20 << 1]);
        model.run(&mut bus);
        assert_eq!(
            model.load(INT_STATUS, Size::B4, T, &mut ledger),
            0,
            "no ENA"
        );

        // `s_i2c_transaction_start` enables NACK, TIME_OUT, TRANS_COMPLETE, ARBITRATION_LOST and
        // END_DETECT; only NACK is raised here.
        model.store(
            INT_ENA,
            Size::B4,
            INT_NACK | INT_TRANS_COMPLETE,
            T,
            &mut ledger,
        );
        assert_eq!(model.load(INT_STATUS, Size::B4, T, &mut ledger), INT_NACK);

        model.store(INT_CLR, Size::B4, INT_NACK, T, &mut ledger);
        assert_eq!(model.load(INT_RAW, Size::B4, T, &mut ledger) & INT_NACK, 0);
        assert_eq!(model.load(INT_STATUS, Size::B4, T, &mut ledger), 0);
        assert_eq!(
            model.load(INT_CLR, Size::B4, T, &mut ledger),
            0,
            "INT_CLR is write-trigger and reads 0"
        );
    }

    #[test]
    fn the_fifo_resets_empty_both_fifos_and_the_counters() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut bus = StubBus::with(&[0x63], &[1, 2, 3]);
        let list = [
            cmd(Op::Restart, 0, false),
            cmd(Op::Write, 1, true),
            cmd(Op::Read, 3, false),
        ];
        start(&mut model, &mut ledger, &list, &[0x63 << 1 | 1]);
        model.run(&mut bus);
        for byte in [0xAAu8, 0xBB] {
            model.store(DATA, Size::B4, u32::from(byte), T, &mut ledger);
        }
        let sr = model.load(SR, Size::B4, T, &mut ledger);
        assert_eq!((sr >> SR_RXFIFO_CNT_SHIFT) & SR_FIFO_CNT_MASK, 3);
        assert_eq!((sr >> SR_TXFIFO_CNT_SHIFT) & SR_FIFO_CNT_MASK, 2);

        model.store(
            FIFO_CONF,
            Size::B4,
            FIFO_CONF_RX_RST | FIFO_CONF_TX_RST,
            T,
            &mut ledger,
        );
        model.store(FIFO_CONF, Size::B4, 0, T, &mut ledger);
        assert!(model.rx_bytes().is_empty() && model.tx_bytes().is_empty());
        let sr = model.load(SR, Size::B4, T, &mut ledger);
        assert_eq!((sr >> SR_RXFIFO_CNT_SHIFT) & SR_FIFO_CNT_MASK, 0);
        assert_eq!((sr >> SR_TXFIFO_CNT_SHIFT) & SR_FIFO_CNT_MASK, 0);
    }

    /// `scl_rst_slv_en` is polled for up to 50 ms after a bus clear; clearing it inside the access
    /// is acceptable. `scl_rst_slv_num` keeps the written value.
    #[test]
    fn scl_rst_slv_en_clears_inside_the_access() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let num = 9 << 1;
        model.store(SCL_SP_CONF, Size::B4, num | 1, T, &mut ledger);
        let val = model.load(SCL_SP_CONF, Size::B4, T, &mut ledger);
        assert_eq!(val & 1, 0, "scl_rst_slv_en read back 0");
        assert_eq!(val & 0x3E, num, "scl_rst_slv_num is kept");
    }

    /// One first touch per register; a narrow access touches only the registers it addresses.
    #[test]
    fn first_touches_are_reported_once_with_the_class_of_the_spec_row() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.load(INT_RAW, Size::B4, VTime(10), &mut ledger);
        model.load(INT_RAW, Size::B1, VTime(20), &mut ledger);
        model.store(INT_ENA, Size::B4, 0, VTime(30), &mut ledger);
        let touches: Vec<_> = ledger
            .first_touches()
            .iter()
            .map(|t| (t.off, t.access, t.now, t.allowlisted))
            .collect();
        assert_eq!(
            touches,
            vec![
                (INT_RAW, TouchAccess::Read, VTime(10), false),
                (INT_ENA, TouchAccess::Write, VTime(30), false),
            ]
        );
        let id = <Model as Peripheral>::ID;
        assert_eq!(
            ledger.class_of(LedgerSubject::Register {
                periph: id,
                off: INT_RAW
            }),
            Fidelity::B
        );
        let unmodeled = ledger.unmodeled().count();

        // An offset with no row reads 0, ignores writes and is reported unclassified once.
        assert_eq!(model.load(0x900, Size::B4, VTime(40), &mut ledger), 0);
        model.store(0x900, Size::B4, 0xFF, VTime(50), &mut ledger);
        assert_eq!(model.load(0x900, Size::B4, VTime(60), &mut ledger), 0);
        assert_eq!(ledger.unmodeled().count(), unmodeled + 1);
    }

    /// A `CPU0_` reset reaches no block, so the block keeps everything.
    #[test]
    fn a_reset_restores_the_registers_and_empties_the_fifos() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(INT_ENA, Size::B4, INT_NACK, T, &mut ledger);
        model.store(DATA, Size::B4, 0x5A, T, &mut ledger);

        let cpu_only = ResetKind {
            cause: ResetCause(0x0C),
            scope: ResetScope::Core,
            fanout: ResetFanout::CpuAndPms,
        };
        model.reset_regs(cpu_only);
        assert_eq!(model.load(INT_ENA, Size::B4, T, &mut ledger), INT_NACK);
        assert_eq!(model.tx_bytes(), [0x5A]);

        let core = ResetKind {
            cause: ResetCause(0x03),
            scope: ResetScope::Core,
            fanout: ResetFanout::AllBlocks,
        };
        model.reset_regs(core);
        assert_eq!(model.load(INT_ENA, Size::B4, T, &mut ledger), 0);
        assert!(model.tx_bytes().is_empty());
        assert_eq!(
            model.reg(CTR),
            0x20B,
            "CTR goes back to its reset value (regs_i2c0)"
        );
    }

    #[test]
    fn the_register_values_round_trip_without_the_static_table() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(
            INT_ENA,
            Size::B4,
            INT_NACK | INT_TRANS_COMPLETE,
            T,
            &mut ledger,
        );
        let saved = regs::store_values(&model.regs);
        assert_eq!(saved.len(), REG_COUNT);
        let restored = regs::store_from(&REGS, &saved);
        assert_eq!(regs::store_values(&restored), saved);
        let short = regs::store_from(&REGS, &saved[..2]);
        assert_eq!(short.get(idx::I2C_CTR), saved[idx::I2C_CTR]);
        assert_eq!(short.get(idx::I2C_DATE), REGS[idx::I2C_DATE].reset);
    }
}
