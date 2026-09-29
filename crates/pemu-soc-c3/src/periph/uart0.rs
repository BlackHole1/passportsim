//! UART0: the ROM banner channel and the two TX-idle polls the boot path spins on
//! (`specs/blocks/uart0.toml`). The device console is USB Serial/JTAG; the app sets
//! `CONSOLE_UART_NUM = -1`.
//!
//! A write to `UART_FIFO` always appends the byte to [`Model::take_tx`]'s ring at once, so console
//! text never depends on pacing. Pacing changes only the register view: [`Model::tx_pending`]
//! counts bytes not yet on the wire, one scheduled event per byte at the programmed baud, and
//! `TXFIFO_CNT` and `ST_UTX_OUT` follow it (the `uart0.txfifo_cnt` and `uart0.st_utx_out` rows).
//!
//! It matters because the ROM waits for its banner to drain (ROM `uart_tx_flush` 0x4004df9e and
//! `uart_tx_wait_idle` 0x4004dfd0); a transmitter that is never busy makes every later console
//! timestamp early by the banner's wire time. [`Model::byte_ps`] decodes `UART_CLKDIV` as the
//! hardware does, so a guest that changes baud is paced at its rate. Pacing is on only under the
//! `device` profile ([`Model::set_tx_pacing`]).
//!
//! RX is a queue ([`Model::push_rx`]) that nothing feeds yet: the Passport exposes USB only.
//!
//! Source 21 follows [`Model::irq_level`], which every `Peripheral` entry point drives.

use std::collections::VecDeque;

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::irq_source::{IrqSource, irq};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::VTime;

use super::block::Uart0;
use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, id};
use crate::r#gen::regs_uart0::{REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

pub const IRQ_SOURCE: IrqSource = irq::UART0;

/// Bytes the TX ring holds before the oldest are dropped. The machine drains it every step, so
/// this only bounds what a guest printing without a reader can cost.
pub const TX_CAPACITY: usize = 8 * 1024;

/// Bytes the RX queue holds; the same bound from the other side.
pub const RX_CAPACITY: usize = 1024;

/// Shift of `UART_STATUS.RXFIFO_CNT`.
const RXFIFO_CNT_SHIFT: u32 = 0;

/// Shift of `UART_STATUS.TXFIFO_CNT`.
const TXFIFO_CNT_SHIFT: u32 = 16;

/// Shift of `UART_FSM_STATUS.ST_UTX_OUT`; the ROM polls `(FSM_STATUS >> 4) & 0xF == 0`.
const ST_UTX_OUT_SHIFT: u32 = 4;

/// The one non-idle `ST_UTX_OUT` code this model produces. Silicon walks several codes; every
/// known consumer compares against 0, so one busy code is enough (class C).
const ST_UTX_OUT_BUSY: u32 = 1;

const TAG_TX: u16 = 0;

/// Bits one byte occupies on the wire: 8N1. The frame is not decoded from `UART_CONF0` because
/// nothing reprograms it (class C).
const BITS_PER_BYTE: u64 = 10;

/// The crystal, `UART_CLK_CONF.SCLK_SEL` 3, in Hz. The ROM selects it before its first banner
/// byte and programs `UART_CLKDIV` 0x0030_015B, 115200 baud from 40 MHz.
pub const SCLK_HZ: u64 = 40_000_000;

/// `UART_CLK_CONF.SCLK_SEL` 1, APB_CLK, in Hz: 80 MHz (TRM 26.3). A peripheral is handed no
/// `Clock` to ask. IDF's driver selects it: the `probe_campaign_timing` capture reads
/// `UART_CLK_CONF` 0x0350_0000 and `UART_CLKDIV` 0x0070_02B6 at 115200 baud.
pub const APB_SCLK_HZ: u64 = 80_000_000;

/// `UART_CLK_CONF.SCLK_SEL`, bits 21:20: 1 APB_CLK, 2 RC_FAST_CLK, 3 XTAL_CLK.
const SCLK_SEL_SHIFT: u32 = 20;

/// `UART_CLK_CONF.SCLK_DIV_NUM`, bits 19:12; `SCLK_DIV_A` (11:6) and `SCLK_DIV_B` (5:0) are the
/// fraction's denominator and numerator.
const SCLK_DIV_NUM_SHIFT: u32 = 12;

/// `UART_CLKDIV.CLKDIV`, bits 11:0 on the C3 (`uart_reg.h:550`), so the slowest baud off the
/// crystal is about 9770.
const CLKDIV_MASK: u32 = 0x0000_0FFF;

/// `UART_CLKDIV.CLKDIV_FRAG`, the divider's sixteenths, bits 23:20.
const CLKDIV_FRAG_SHIFT: u32 = 20;

const CLKDIV_FRAG_MASK: u32 = 0xF;

const PS_PER_S: u64 = 1_000_000_000_000;

/// Ten-bit width of both FIFO counters.
const FIFO_CNT_MASK: u32 = 0x3FF;

/// Bytes the transmit FIFO holds. `TXFIFO_CNT` is clamped here, not at the field width, because
/// IDF's `uart_ll_get_txfifo_len()` is `128 - txfifo_cnt` unsigned: a larger count wraps to about
/// 4e9 free bytes. Silicon refuses the write past the depth, which this model does not.
const TX_FIFO_DEPTH: u32 = 128;

/// `UART_INT_RAW.TXFIFO_EMPTY`: set while the transmit FIFO is empty.
const INT_TXFIFO_EMPTY: u32 = 1 << 1;

const INT_RXFIFO_FULL: u32 = 1 << 0;

/// `UART_INT_RAW.TX_DONE`, bit 14: set when the transmitter sends its last byte, kept until
/// `INT_CLR`. `uart_wait_tx_done` waits for it whenever the transmitter is busy; without it a
/// paced transmitter times out after 200 ms. Class A from the `probe_campaign_timing` capture.
const INT_TX_DONE: u32 = 1 << 14;

/// `UART_ID.UPDATE`, which self-clears.
const ID_UPDATE: u32 = 1 << 31;

const TOUCH_WORDS: usize = REG_COUNT.div_ceil(64);

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    touched: [u64; TOUCH_WORDS],
    tx: VecDeque<u8>,
    /// Bytes the ring dropped because nothing drained it.
    tx_dropped: u64,
    rx: VecDeque<u8>,
    /// Bytes accepted and not yet on the wire: FIFO occupancy plus the shift register. 0 while
    /// pacing is off.
    tx_pending: u32,
    /// The pending per-byte shift-out event, armed while `tx_pending > 0`.
    tx_event: Option<EventHandle>,
    /// Whether the transmitter is paced at the `UART_CLKDIV` baud ([`Model::set_tx_pacing`]).
    tx_paced: bool,
}

impl Default for Model {
    fn default() -> Self {
        let mut m = Model {
            regs: RegStore::new(&REGS),
            touched: [0; TOUCH_WORDS],
            tx: VecDeque::new(),
            tx_dropped: 0,
            rx: VecDeque::new(),
            tx_pending: 0,
            tx_event: None,
            tx_paced: false,
        };
        m.refresh_status();
        m
    }
}

fn key(tag: u16) -> EventKey {
    EventKey {
        owner: Owner::Periph(id::UART0),
        tag,
    }
}

impl Model {
    pub fn take_tx(&mut self) -> Vec<u8> {
        self.tx.drain(..).collect()
    }

    pub fn tx_len(&self) -> usize {
        self.tx.len()
    }

    pub fn tx_dropped(&self) -> u64 {
        self.tx_dropped
    }

    pub fn tx_pending(&self) -> u32 {
        self.tx_pending
    }

    pub fn tx_paced(&self) -> bool {
        self.tx_paced
    }

    /// Turns pacing on (`device` profile) or off (`fast`). Turning it off finishes whatever was in
    /// flight, so no scheduled event outlives the setting.
    pub fn set_tx_pacing(&mut self, paced: bool, sched: &mut Scheduler) {
        if paced == self.tx_paced {
            return;
        }
        self.tx_paced = paced;
        if !paced {
            if self.tx_pending > 0 {
                self.raise_tx_done();
            }
            self.tx_pending = 0;
            if let Some(h) = self.tx_event.take() {
                sched.cancel(h);
            }
            self.refresh_status();
        }
    }

    /// Wire time of one byte at the `UART_CLKDIV` baud, or `None` when the divider is 0 or
    /// `SCLK_SEL` 0 names no clock.
    ///
    /// The divider is `CLKDIV + CLKDIV_FRAG / 16` core-clock periods per bit; the core clock is
    /// the selected source divided by `SCLK_DIV_NUM + 1 + SCLK_DIV_B / SCLK_DIV_A` (IDF
    /// `uart_ll_set_baudrate`). Class A for the APB setting; RC_FAST is class C.
    ///
    /// Integer arithmetic, rounding down, so every host agrees. `u128` so the no-panic guarantee
    /// does not rest on [`CLKDIV_MASK`].
    pub fn byte_ps(&self) -> Option<u64> {
        let clkdiv = self.regs.get(idx::UART_CLKDIV);
        let sixteenths = u128::from(clkdiv & CLKDIV_MASK) * 16
            + u128::from((clkdiv >> CLKDIV_FRAG_SHIFT) & CLKDIV_FRAG_MASK);
        if sixteenths == 0 {
            return None;
        }
        let conf = self.regs.get(idx::UART_CLK_CONF);
        let source = match (conf >> SCLK_SEL_SHIFT) & 3 {
            1 => APB_SCLK_HZ,
            2 => super::timg::RC_FAST_HZ,
            3 => SCLK_HZ,
            _ => return None,
        };
        let num = u128::from((conf >> SCLK_DIV_NUM_SHIFT) & 0xFF) + 1;
        let (div_a, div_b) = (u128::from((conf >> 6) & 0x3F), u128::from(conf & 0x3F));
        // The core clock's divisor as a fraction `divisor_n / divisor_d`.
        let (divisor_n, divisor_d) = if div_a == 0 {
            (num, 1)
        } else {
            (num * div_a + div_b, div_a)
        };
        let ps = u128::from(BITS_PER_BYTE) * sixteenths * u128::from(PS_PER_S) * divisor_n
            / (16 * u128::from(source) * divisor_d);
        Some(u64::try_from(ps).unwrap_or(u64::MAX))
    }

    /// Arms the per-byte shift-out event while the transmitter has work and the pacing is on.
    fn arm_tx(&mut self, now: VTime, sched: &mut Scheduler) {
        if self.tx_event.is_some() || self.tx_pending == 0 {
            return;
        }
        let Some(ps) = self.byte_ps() else {
            // An undefined baud cannot be paced; the bytes are already in the ring.
            self.tx_pending = 0;
            self.raise_tx_done();
            self.refresh_status();
            return;
        };
        let at = VTime(now.0.saturating_add(ps));
        self.tx_event = Some(sched.schedule(now, at, key(TAG_TX)));
    }

    pub fn tick(&mut self, tag: u16, now: VTime, sched: &mut Scheduler) -> Wiring {
        if tag != TAG_TX {
            return Wiring::None;
        }
        self.tx_event = None;
        self.tx_pending = self.tx_pending.saturating_sub(1);
        if self.tx_pending == 0 {
            self.raise_tx_done();
        }
        self.arm_tx(now, sched);
        self.refresh_status();
        Wiring::None
    }

    /// The transmitter sent its last byte and is idle: [`INT_TX_DONE`] latches.
    fn raise_tx_done(&mut self) {
        let raw = self.regs.get(idx::UART_INT_RAW) | INT_TX_DONE;
        self.regs.set(idx::UART_INT_RAW, raw);
    }

    /// Queues bytes for the guest to read out of `UART_FIFO`, dropping what does not fit.
    pub fn push_rx(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if self.rx.len() >= RX_CAPACITY {
                break;
            }
            self.rx.push_back(*byte);
        }
        self.refresh_status();
    }

    pub fn irq_level(&self) -> bool {
        self.regs.get(idx::UART_INT_ST) != 0
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    /// Restores the registers this reset clears and empties both queues and the transmitter,
    /// cancelling its event. Pacing is the machine's profile, not block state, so it survives.
    pub fn reset_to(&mut self, kind: ResetKind, sched: &mut Scheduler) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.tx.clear();
        self.rx.clear();
        self.tx_pending = 0;
        if let Some(h) = self.tx_event.take() {
            sched.cancel(h);
        }
        self.refresh_status();
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Read, at, ledger);
        if i == idx::UART_FIFO && byte == 0 {
            let next = self.rx.pop_front().unwrap_or(0);
            self.regs.set(i, u32::from(next));
            self.refresh_status();
        }
        self.regs.read(i, byte, size)
    }

    /// Writes the low `size` bytes of `val` at block offset `off`. Only a write to byte 0 of
    /// `UART_FIFO` transmits.
    pub fn store(
        &mut self,
        off: u32,
        size: Size,
        val: u32,
        now: VTime,
        sched: &mut Scheduler,
        ledger: &mut FidelityLedger,
    ) {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte)) = regs::reg_at(&REGS, off) else {
            regs::hole(off, TouchAccess::Write, at, ledger);
            return;
        };
        regs::touch(&mut self.touched, &REGS, i, TouchAccess::Write, at, ledger);
        if i == idx::UART_FIFO {
            if byte == 0 {
                self.transmit((val & 0xFF) as u8, now, sched);
            }
            return;
        }
        let delta = self.regs.write(i, byte, size, val);
        match i {
            idx::UART_ID => self.regs.clear_sc(i, ID_UPDATE),
            idx::UART_INT_CLR => {
                // The clear bits are write-triggers; `Delta::triggers` carries the bits written 1.
                let raw = self.regs.get(idx::UART_INT_RAW) & !delta.triggers;
                self.regs.set(idx::UART_INT_RAW, raw);
                self.refresh_status();
            }
            idx::UART_INT_ENA => self.refresh_status(),
            _ => {}
        }
    }

    /// Appends one byte to the TX ring and, while pacing is on, hands it to the transmitter.
    /// Nothing is refused past the 128-byte depth: `TXFIFO_CNT` reports what is queued, which makes
    /// the ROM's `while TXFIFO_CNT >= 126` check wait, and no text is lost (class C; silicon
    /// would drop the byte).
    fn transmit(&mut self, byte: u8, now: VTime, sched: &mut Scheduler) {
        if self.tx.len() >= TX_CAPACITY {
            self.tx.pop_front();
            self.tx_dropped = self.tx_dropped.saturating_add(1);
        }
        self.tx.push_back(byte);
        if self.tx_paced {
            self.tx_pending = self.tx_pending.saturating_add(1);
            self.arm_tx(now, sched);
        } else {
            self.raise_tx_done();
        }
        self.refresh_status();
    }

    /// Recomputes the two status registers and the interrupt status from the queues.
    fn refresh_status(&mut self) {
        let status = self.regs.get(idx::UART_STATUS)
            & !((FIFO_CNT_MASK << RXFIFO_CNT_SHIFT) | (FIFO_CNT_MASK << TXFIFO_CNT_SHIFT));
        let rx_cnt = (self.rx.len() as u32).min(FIFO_CNT_MASK);
        // The byte in the shift register has left the FIFO, so the FIFO holds one fewer than the
        // transmitter owes. Clamped at the depth (see `TX_FIFO_DEPTH`).
        let tx_cnt = self.tx_pending.saturating_sub(1).min(TX_FIFO_DEPTH);
        self.regs.set(
            idx::UART_STATUS,
            status | (rx_cnt << RXFIFO_CNT_SHIFT) | (tx_cnt << TXFIFO_CNT_SHIFT),
        );
        let utx = if self.tx_pending > 0 {
            ST_UTX_OUT_BUSY << ST_UTX_OUT_SHIFT
        } else {
            0
        };
        self.regs.set(idx::UART_FSM_STATUS, utx);

        let mut raw = self.regs.get(idx::UART_INT_RAW);
        if tx_cnt == 0 {
            raw |= INT_TXFIFO_EMPTY;
        } else {
            raw &= !INT_TXFIFO_EMPTY;
        }
        if self.rx.is_empty() {
            raw &= !INT_RXFIFO_FULL;
        }
        self.regs.set(idx::UART_INT_RAW, raw);
        let st = raw & self.regs.get(idx::UART_INT_ENA);
        self.regs.set(idx::UART_INT_ST, st);
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <Uart0 as Block>::ID;
    const BASE: u32 = <Uart0 as Block>::BASE;
    const SIZE: u32 = <Uart0 as Block>::SIZE;

    fn reset(&mut self, kind: ResetKind, cx: &mut Cx) {
        self.reset_to(kind, cx.sched);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        let val = self.load(off, size, cx.now, cx.ledger);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
        RegRead { val, stop: false }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        self.store(off, size, val, cx.now, cx.sched, cx.ledger);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
        RegWrite {
            stop: false,
            wiring: Wiring::None,
        }
    }

    fn on_event(&mut self, tag: u16, cx: &mut Cx) -> Wiring {
        let wiring = self.tick(tag, cx.now, cx.sched);
        cx.irq.set_source(IRQ_SOURCE, self.irq_level());
        wiring
    }

    /// While the transmitter is busy, the registers a byte in flight moves hold until the next
    /// event, so the ROM's TX-idle poll fast-forwards to the drain.
    fn stable_until(&self, off: u32, cx: &Cx) -> Stability {
        let _ = cx;
        match regs::reg_at(&REGS, off).map(|(i, _)| i) {
            Some(
                idx::UART_STATUS | idx::UART_FSM_STATUS | idx::UART_INT_RAW | idx::UART_INT_ST,
            ) if self.tx_event.is_some() => Stability::UntilNextEvent,
            _ => Stability::UntilInput,
        }
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

crate::regs::store_serde!();

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::reset::{ResetCause, ResetKind};

    const OFF_FIFO: u32 = 0x000;
    const OFF_INT_RAW: u32 = 0x004;
    const OFF_INT_ST: u32 = 0x008;
    const OFF_INT_ENA: u32 = 0x00C;
    const OFF_INT_CLR: u32 = 0x010;
    const OFF_CLKDIV: u32 = 0x014;
    const OFF_STATUS: u32 = 0x01C;
    const OFF_FSM_STATUS: u32 = 0x06C;
    const OFF_DATE: u32 = 0x07C;
    const OFF_ID: u32 = 0x080;

    const T: VTime = VTime(3);

    fn model() -> (Model, Scheduler, FidelityLedger) {
        (
            Model::default(),
            Scheduler::new(),
            FidelityLedger::default(),
        )
    }

    /// `UART_CLK_CONF` as the ROM leaves it: the crystal, `SCLK_DIV_NUM` 0 (the reset value's 1
    /// would halve the core clock).
    const ROM_CLK_CONF: u32 = 0x0370_0000;

    const OFF_CLK_CONF: u32 = 0x078;

    /// A model paced at the `UART_CLKDIV` baud with the ROM's core clock.
    fn paced() -> (Model, Scheduler, FidelityLedger) {
        let (mut m, mut s, mut l) = model();
        m.set_tx_pacing(true, &mut s);
        m.store(OFF_CLK_CONF, Size::B4, ROM_CLK_CONF, T, &mut s, &mut l);
        (m, s, l)
    }

    /// The model never sets `RXFIFO_FULL` itself, so the test sets the raw bit.
    #[test]
    fn a_fifo_read_that_empties_rx_lowers_source_21() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut m = Model::default();
        m.push_rx(b"z");
        m.regs
            .set(idx::UART_INT_RAW, INT_RXFIFO_FULL | INT_TXFIFO_EMPTY);
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA, Size::B4, INT_RXFIFO_FULL, cx));
        assert!(
            p.source(IRQ_SOURCE),
            "RXFIFO_FULL enabled asserts source 21"
        );
        let byte = p
            .with(|cx| Peripheral::read(&mut m, OFF_FIFO, Size::B4, cx))
            .val;
        assert_eq!(byte, u32::from(b'z'));
        assert!(
            !p.source(IRQ_SOURCE),
            "the read that empties the queue lowers it"
        );
    }

    /// The TX FIFO is always empty here, so enabling `TXFIFO_EMPTY` raises the source.
    #[test]
    fn int_st_drives_source_21_through_the_entry_points() {
        use crate::intc::testing::TestPorts;
        let mut p = TestPorts::new();
        let mut m = Model::default();
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA, Size::B4, INT_TXFIFO_EMPTY, cx));
        assert!(
            p.source(IRQ_SOURCE),
            "TXFIFO_EMPTY enabled asserts source 21"
        );
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA, Size::B4, 0, cx));
        assert!(!p.source(IRQ_SOURCE), "INT_ENA 0 lowers it");
        p.with(|cx| Peripheral::write(&mut m, OFF_INT_ENA, Size::B4, INT_TXFIFO_EMPTY, cx));
        assert!(p.source(IRQ_SOURCE));
        p.with(|cx| Peripheral::reset(&mut m, ResetKind::of(ResetCause::RTC_SW_SYS).unwrap(), cx));
        assert!(!p.source(IRQ_SOURCE), "a system reset lowers it");
    }

    /// Under `device` a TX-idle poll sees busy until the byte's shift-out event, idle at it.
    #[test]
    fn a_paced_byte_keeps_both_tx_idle_polls_busy_until_its_drain() {
        let (mut m, mut s, mut l) = paced();
        let ps = m.byte_ps().expect("the reset divider is not 0");
        m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);

        assert_eq!(m.tx_pending(), 1);
        assert_eq!(
            (m.load(OFF_FSM_STATUS, Size::B4, T, &mut l) >> 4) & 0xF,
            ST_UTX_OUT_BUSY,
            "ST_UTX_OUT reads busy while the byte is on the wire"
        );
        assert_eq!(
            m.load(OFF_INT_RAW, Size::B4, T, &mut l) & INT_TXFIFO_EMPTY,
            INT_TXFIFO_EMPTY,
            "the FIFO itself is empty: the byte is in the shift register"
        );
        assert_eq!(m.tx_len(), 1);

        let at = VTime(T.0 + ps);
        assert_eq!(s.len(), 1, "the shift-out event is armed");
        assert_eq!(s.pop_due(VTime(at.0 - 1)), None, "and not before its time");
        assert_eq!(s.pop_due(at), Some(key(TAG_TX)));
        assert!(
            matches!(
                crate::intc::testing::TestPorts::new()
                    .with(|cx| Peripheral::stable_until(&m, OFF_STATUS, cx)),
                Stability::UntilNextEvent
            ),
            "so the ROM poll can fast-forward to the drain"
        );

        assert!(matches!(m.tick(TAG_TX, at, &mut s), Wiring::None));
        assert_eq!(m.tx_pending(), 0);
        assert_eq!(m.load(OFF_STATUS, Size::B4, at, &mut l) & 0x03FF_0000, 0);
        assert_eq!((m.load(OFF_FSM_STATUS, Size::B4, at, &mut l) >> 4) & 0xF, 0);
        assert_eq!(s.len(), 0, "nothing is left to send");
    }

    /// [`INT_TX_DONE`] latches when the paced transmitter sends its last byte, stays until
    /// `INT_CLR`, and drives source 21 while enabled. Unpaced it latches at the write.
    #[test]
    fn tx_done_latches_when_the_last_byte_leaves() {
        let (mut m, mut s, mut l) = paced();
        let ps = m.byte_ps().expect("the reset divider is not 0");
        m.store(OFF_FIFO, Size::B1, u32::from(b'a'), T, &mut s, &mut l);
        m.store(OFF_FIFO, Size::B1, u32::from(b'b'), T, &mut s, &mut l);
        m.store(OFF_INT_ENA, Size::B4, INT_TX_DONE, T, &mut s, &mut l);
        let raw = |m: &mut Model, l: &mut FidelityLedger, at| {
            m.load(OFF_INT_RAW, Size::B4, at, l) & INT_TX_DONE
        };
        assert_eq!(raw(&mut m, &mut l, T), 0, "two bytes still to send");
        let first = VTime(T.0 + ps);
        assert_eq!(s.pop_due(first), Some(key(TAG_TX)));
        m.tick(TAG_TX, first, &mut s);
        assert_eq!(raw(&mut m, &mut l, first), 0, "one byte still to send");
        assert!(!m.irq_level());
        let second = VTime(first.0 + ps);
        assert_eq!(s.pop_due(second), Some(key(TAG_TX)));
        m.tick(TAG_TX, second, &mut s);
        assert_eq!(
            raw(&mut m, &mut l, second),
            INT_TX_DONE,
            "the last byte left"
        );
        assert!(m.irq_level(), "enabled, it drives source 21");
        m.store(OFF_INT_CLR, Size::B4, INT_TX_DONE, second, &mut s, &mut l);
        assert_eq!(raw(&mut m, &mut l, second), 0, "until INT_CLR");
        assert!(!m.irq_level());

        let (mut m, mut s, mut l) = model();
        m.store(OFF_FIFO, Size::B1, u32::from(b'c'), T, &mut s, &mut l);
        assert_eq!(raw(&mut m, &mut l, T), INT_TX_DONE, "unpaced, at the write");
    }

    /// A burst longer than the FIFO reports what is queued and costs one byte time per byte.
    #[test]
    fn a_burst_drains_one_byte_per_baud_period_and_reports_what_is_queued() {
        let (mut m, mut s, mut l) = paced();
        let ps = m.byte_ps().expect("the reset divider is not 0");
        let line = b"ESP-ROM:esp32c3-eco7-20230720\r\n";
        for byte in line {
            m.store(OFF_FIFO, Size::B1, u32::from(*byte), T, &mut s, &mut l);
        }
        assert_eq!(m.tx_pending(), line.len() as u32);
        assert_eq!(
            m.load(OFF_STATUS, Size::B4, T, &mut l) >> 16 & FIFO_CNT_MASK,
            line.len() as u32 - 1,
            "the byte in the shift register has left the FIFO"
        );

        let mut now = T;
        for left in (0..line.len()).rev() {
            assert_eq!(s.len(), 1, "one event at a time, one per byte");
            now = VTime(now.0 + ps);
            assert_eq!(s.pop_due(now), Some(key(TAG_TX)));
            m.tick(TAG_TX, now, &mut s);
            assert_eq!(m.tx_pending(), left as u32);
        }
        assert_eq!(
            now,
            VTime(T.0 + ps * line.len() as u64),
            "the burst took its transmission time"
        );
        assert_eq!(m.take_tx(), line.to_vec(), "and lost nothing");
    }

    /// 0x0030_015B is `(40e6 << 4) / 115200` = 5555 sixteenths.
    #[test]
    fn the_rom_baud_divider_decodes_to_115200_from_the_crystal() {
        let (mut m, mut s, mut l) = paced();
        m.store(OFF_CLKDIV, Size::B4, 0x0030_015B, T, &mut s, &mut l);
        let ps = m.byte_ps().expect("a programmed divider");
        assert_eq!(ps, 86_796_875, "10 bits at 115200 baud is 86.8 us");
        m.store(OFF_CLKDIV, Size::B4, 0x0060_002B, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), Some(10_843_750));
        m.store(OFF_CLKDIV, Size::B4, 0, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), None);
        m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);
        assert_eq!(m.tx_pending(), 0);
        assert_eq!(
            m.take_tx(),
            b"x".to_vec(),
            "the byte still reaches the host"
        );
    }

    /// IDF's driver selects APB_CLK (0x0350_0000) and programs 694 + 7/16 for 115200 baud and
    /// 86 + 12/16 for 921600, which are 80 MHz rates (the `probe_campaign_timing` capture).
    #[test]
    fn the_core_clock_follows_the_source_selector_and_its_divider() {
        let (mut m, mut s, mut l) = paced();
        m.store(OFF_CLK_CONF, Size::B4, 0x0350_0000, T, &mut s, &mut l);
        m.store(OFF_CLKDIV, Size::B4, 0x0070_02B6, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), Some(86_804_687), "115200 baud off APB_CLK");
        m.store(OFF_CLKDIV, Size::B4, 0x00C0_0056, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), Some(10_843_750), "921600 baud off APB_CLK");
        // 128 bytes at 115200 are 11.11 ms on the wire, the device's 11187 us less the driver.
        assert_eq!(86_804_687u64 * 128 / 1_000_000, 11_110);
        m.store(OFF_CLK_CONF, Size::B4, 0x0370_1000, T, &mut s, &mut l);
        m.store(OFF_CLKDIV, Size::B4, 0x0030_015B, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), Some(2 * 86_796_875));
        m.store(
            OFF_CLK_CONF,
            Size::B4,
            0x0370_0000 | 2 << 6 | 1,
            T,
            &mut s,
            &mut l,
        );
        assert_eq!(m.byte_ps(), Some(86_796_875 * 3 / 2));
        m.store(OFF_CLK_CONF, Size::B4, 0x0340_0000, T, &mut s, &mut l);
        assert_eq!(m.byte_ps(), None);
    }

    /// The widest divider the guest can reach must still give a byte time without overflowing.
    #[test]
    fn the_widest_divider_the_guest_can_write_still_has_a_byte_time() {
        let (mut m, mut s, mut l) = paced();
        m.store(OFF_CLKDIV, Size::B4, 0xFFFF_FFFF, T, &mut s, &mut l);
        let ps = m.byte_ps().expect("the widest divider still has a rate");
        let expected = 10u128 * (16 * 0x0000_0FFF + 0xF) * 1_000_000_000_000 / (16 * 40_000_000);
        assert_eq!(u128::from(ps), expected, "about 16 ms per byte");
        m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);
        assert_eq!(m.tx_pending(), 1);
        let at = VTime(T.0 + ps);
        assert_eq!(s.pop_due(at), Some(key(TAG_TX)));
        m.tick(TAG_TX, at, &mut s);
        assert_eq!(m.tx_pending(), 0);
    }

    #[test]
    fn a_queue_deeper_than_the_fifo_never_reports_more_than_its_depth() {
        let (mut m, mut s, mut l) = paced();
        for _ in 0..(TX_FIFO_DEPTH + 64) {
            m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);
        }
        assert_eq!(m.tx_pending(), TX_FIFO_DEPTH + 64, "all of it is owed");
        let cnt = m.load(OFF_STATUS, Size::B4, T, &mut l) >> TXFIFO_CNT_SHIFT & FIFO_CNT_MASK;
        assert_eq!(
            cnt, TX_FIFO_DEPTH,
            "but the FIFO never reports past its depth"
        );
        assert_eq!(
            TX_FIFO_DEPTH - cnt,
            0,
            "so the guest's own free-space subtraction stays at 0 and does not wrap"
        );
    }

    #[test]
    fn a_reset_empties_the_paced_transmitter_and_keeps_the_pacing() {
        let (mut m, mut s, mut l) = paced();
        m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);
        assert_eq!(s.len(), 1);
        m.reset_to(
            ResetKind::of(ResetCause::RTC_SW_SYS).expect("a documented reset cause"),
            &mut s,
        );
        assert_eq!(m.tx_pending(), 0);
        assert_eq!(s.len(), 0, "the shift-out event went with the block");
        assert!(m.tx_paced(), "the profile outlives the reset");
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l), 0xE000_C000);

        m.store(OFF_FIFO, Size::B1, u32::from(b'y'), T, &mut s, &mut l);
        m.set_tx_pacing(false, &mut s);
        assert_eq!(s.len(), 0);
        assert_eq!(m.tx_pending(), 0);
        assert_eq!((m.load(OFF_FSM_STATUS, Size::B4, T, &mut l) >> 4) & 0xF, 0);
    }

    #[test]
    fn identity_matches_the_c3_devices_row() {
        assert_eq!(<Model as Peripheral>::ID, <Uart0 as Block>::ID);
        assert_eq!(<Model as Peripheral>::BASE, 0x6000_0000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
    }

    /// Under `fast` both rows are `same_access`: ROM `uart_tx_flush` polls `STATUS & 0x03FF0000`
    /// and `uart_tx_wait_idle` polls `(FSM_STATUS >> 4) & 0xF`.
    #[test]
    fn wait_rows_txfifo_cnt_and_st_utx_out_read_idle_after_a_burst() {
        let (mut m, mut s, mut l) = model();
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l), 0xE000_C000);
        for byte in b"ESP-ROM:esp32c3-eco7-20230720\r\n" {
            m.store(OFF_FIFO, Size::B4, u32::from(*byte), T, &mut s, &mut l);
            let status = m.load(OFF_STATUS, Size::B4, T, &mut l);
            assert_eq!(status & 0x03FF_0000, 0, "TXFIFO_CNT after {byte:#04X}");
            let fsm = m.load(OFF_FSM_STATUS, Size::B4, T, &mut l);
            assert_eq!((fsm >> 4) & 0xF, 0, "ST_UTX_OUT after {byte:#04X}");
        }
        assert_eq!(m.tx_len(), 31);
    }

    #[test]
    fn the_tx_ring_hands_every_byte_over_once() {
        let (mut m, mut s, mut l) = model();
        for byte in b"boot:0xa" {
            m.store(OFF_FIFO, Size::B1, u32::from(*byte), T, &mut s, &mut l);
        }
        assert_eq!(m.take_tx(), b"boot:0xa".to_vec());
        assert!(m.take_tx().is_empty());
        assert_eq!(m.tx_dropped(), 0);
    }

    #[test]
    fn only_the_low_byte_of_the_fifo_register_transmits() {
        let (mut m, mut s, mut l) = model();
        m.store(OFF_FIFO, Size::B4, 0x4142_4344, T, &mut s, &mut l);
        m.store(OFF_FIFO + 1, Size::B1, 0x58, T, &mut s, &mut l);
        m.store(OFF_FIFO + 3, Size::B1, 0x59, T, &mut s, &mut l);
        assert_eq!(m.take_tx(), vec![0x44]);
    }

    #[test]
    fn the_tx_ring_is_bounded_and_counts_what_it_drops() {
        let (mut m, mut s, mut l) = model();
        for i in 0..TX_CAPACITY + 4 {
            m.store(OFF_FIFO, Size::B1, (i & 0xFF) as u32, T, &mut s, &mut l);
        }
        assert_eq!(m.tx_len(), TX_CAPACITY);
        assert_eq!(m.tx_dropped(), 4);
        let out = m.take_tx();
        assert_eq!(out.len(), TX_CAPACITY);
        assert_eq!(out[0], 4, "the oldest four bytes went");
    }

    #[test]
    fn the_interrupt_registers_follow_raw_and_ena() {
        let (mut m, mut s, mut l) = model();
        assert_eq!(
            m.load(OFF_INT_RAW, Size::B4, T, &mut l),
            INT_TXFIFO_EMPTY,
            "the reset value and the steady state"
        );
        assert_eq!(m.load(OFF_INT_ST, Size::B4, T, &mut l), 0);
        assert!(!m.irq_level());

        m.store(OFF_INT_ENA, Size::B4, INT_TXFIFO_EMPTY, T, &mut s, &mut l);
        assert_eq!(m.load(OFF_INT_ST, Size::B4, T, &mut l), INT_TXFIFO_EMPTY);
        assert!(m.irq_level());

        m.store(OFF_INT_CLR, Size::B4, 0xFFFF_FFFF, T, &mut s, &mut l);
        assert_eq!(m.load(OFF_INT_CLR, Size::B4, T, &mut l), 0, "write trigger");
        assert_eq!(m.load(OFF_INT_RAW, Size::B4, T, &mut l), INT_TXFIFO_EMPTY);

        m.store(OFF_INT_ENA, Size::B4, 0, T, &mut s, &mut l);
        assert!(!m.irq_level());
    }

    #[test]
    fn the_id_update_bit_self_clears() {
        let (mut m, mut s, mut l) = model();
        assert_eq!(m.load(OFF_ID, Size::B4, T, &mut l), 0x4000_0500);
        m.store(OFF_ID, Size::B4, 0x4000_0500 | ID_UPDATE, T, &mut s, &mut l);
        assert_eq!(m.load(OFF_ID, Size::B4, T, &mut l) & ID_UPDATE, 0);
    }

    #[test]
    fn the_receive_queue_drives_the_status_counter() {
        let (mut m, _s, mut l) = model();
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l) & FIFO_CNT_MASK, 0);
        m.push_rx(b"AT\r");
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l) & FIFO_CNT_MASK, 3);
        assert_eq!(m.load(OFF_FIFO, Size::B4, T, &mut l), u32::from(b'A'));
        assert_eq!(m.load(OFF_FIFO, Size::B4, T, &mut l), u32::from(b'T'));
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l) & FIFO_CNT_MASK, 1);
        assert_eq!(m.load(OFF_FIFO, Size::B4, T, &mut l), u32::from(b'\r'));
        assert_eq!(m.load(OFF_FIFO, Size::B4, T, &mut l), 0, "an empty queue");
    }

    /// esptool's `CHANGE_BAUDRATE` writes `UART_CLKDIV`.
    #[test]
    fn the_configuration_registers_are_storage() {
        let (mut m, mut s, mut l) = model();
        assert_eq!(m.load(OFF_CLKDIV, Size::B4, T, &mut l), 0x2B6);
        assert_eq!(m.load(OFF_DATE, Size::B4, T, &mut l), 0x0200_8270);
        m.store(OFF_CLKDIV, Size::B4, 0x1B2, T, &mut s, &mut l);
        assert_eq!(m.load(OFF_CLKDIV, Size::B4, T, &mut l), 0x1B2);
    }

    /// A core reset restores UART0 and empties the ring; a CPU reset keeps both.
    #[test]
    fn reset_scope_matrix_over_a_digital_block() {
        let (mut m, mut s, mut l) = model();
        let kind = |c| ResetKind::of(c).expect("a documented reset cause");
        m.store(OFF_CLKDIV, Size::B4, 0x1B2, T, &mut s, &mut l);
        m.store(OFF_FIFO, Size::B1, u32::from(b'x'), T, &mut s, &mut l);

        m.reset_to(kind(ResetCause::RTC_SW_CPU), &mut s);
        assert_eq!(m.load(OFF_CLKDIV, Size::B4, T, &mut l), 0x1B2);
        assert_eq!(m.tx_len(), 1, "a CPU reset keeps the block");

        m.reset_to(kind(ResetCause::RTC_SW_SYS), &mut s);
        assert_eq!(m.load(OFF_CLKDIV, Size::B4, T, &mut l), 0x2B6);
        assert_eq!(m.tx_len(), 0);
        assert_eq!(m.load(OFF_STATUS, Size::B4, T, &mut l), 0xE000_C000);
    }

    #[test]
    fn fidelity_comes_from_the_generated_table() {
        let m = Model::default();
        assert_eq!(m.fidelity(OFF_STATUS), Fidelity::B);
        assert_eq!(m.fidelity(OFF_FSM_STATUS), Fidelity::B);
        assert_eq!(m.fidelity(OFF_FIFO), Fidelity::B);
        assert_eq!(
            m.fidelity(OFF_CLKDIV),
            Fidelity::C,
            "the baud rate is stored and not honored, specs/blocks/uart0.toml"
        );
    }
}
