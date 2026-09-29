//! I2S0: registers, the clock registers resolved to a sample rate, and DMA pacing
//! (`specs/blocks/i2s0.toml`).
//!
//! 1. `i2s_ll_tx_update` and `i2s_ll_rx_update` write the update bit and spin until it reads 0
//!    (IDF `i2s_ll.h:398`, `:409`); without the self-clear `bsp_audio_init` never returns.
//! 2. [`Model::fs_hz`] reconstructs the sample rate from the fractional MCLK and BCK dividers
//!    (160e6 / 39.0625 / 8 / 32 = 16000); the codec's `PcmFormat` and the pacing derive from it.
//! 3. While a direction and its GDMA pair are started, one descriptor of PCM moves per period.
//!    Periods are placed with `pemu_core::time::frame_time` from the start time, so they are
//!    spaced exactly `frames x 1e12 / fs` ps with no accumulated drift.
//!
//! The descriptors belong to GDMA; [`I2sDma`] is what this file needs from them. The I2S
//! interrupt (source 20) is unused: completion comes from the GDMA EOF interrupt.

use pemu_core::fidelity::{Fidelity, FidelityLedger, TouchAccess};
use pemu_core::regstore::{RegStore, Size};
use pemu_core::reset::ResetKind;
use pemu_core::sched::{EventHandle, EventKey, Owner, PeriphId, Scheduler};
use pemu_core::serde::{Deserialize, Serialize};
use pemu_core::time::{VTime, frame_time};

use pemu_board::traits::PcmFormat;

use crate::r#gen::regs_i2s0::{BLOCK_SIZE, REG_COUNT, REGS, idx};
use crate::regs::{self, TouchAt};

use super::{Block, Cx, Peripheral, RegRead, RegWrite, Stability, Wiring, block};

/// Direction of an I2S period carried by `Wiring::I2sPeriod`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Dir {
    /// Transmit: frames to the codec DAC.
    Tx,
    /// Receive: frames from the codec ADC.
    Rx,
}

impl Dir {
    pub const ALL: [Dir; 2] = [Dir::Tx, Dir::Rx];

    /// The owner-local event tag of this direction's pacing event.
    pub const fn tag(self) -> u16 {
        match self {
            Dir::Tx => 0,
            Dir::Rx => 1,
        }
    }

    /// The direction of an event tag, or `None` for a tag this block never scheduled.
    pub const fn of_tag(tag: u16) -> Option<Dir> {
        match tag {
            0 => Some(Dir::Tx),
            1 => Some(Dir::Rx),
            _ => None,
        }
    }
}

const CONF_START: u32 = 1 << 2;
/// `*_CONF.update`, bit 8; written 1 and polled until it reads 0.
const CONF_UPDATE: u32 = 1 << 8;
/// `*_CONF.reset`, bit 0, and `*_CONF.fifo_reset`, bit 1; both pulsed 1 then 0.
const CONF_RESET: u32 = (1 << 0) | (1 << 1);
/// `RX_CONF.rx_mono`, bit 5: one slot per frame in the DMA buffer.
const RX_CONF_MONO: u32 = 1 << 5;
/// `RX_CONF.rx_slave_mod`, bit 3: the RX channel follows the TX clock.
const RX_CONF_SLAVE_MOD: u32 = 1 << 3;
/// `TX_CONF.sig_loopback`, bit 27: RX shares the TX BCK and WS.
const TX_CONF_SIG_LOOPBACK: u32 = 1 << 27;

const STATE_TX_IDLE: u32 = 1 << 0;

/// `*_CONF1.bck_div_num`, bits [12:7].
const CONF1_BCK_DIV_SHIFT: u32 = 7;
/// `*_CONF1.bits_mod`, bits [17:13].
const CONF1_BITS_MOD_SHIFT: u32 = 13;
/// `*_CONF1.tdm_chan_bits`, bits [28:24].
const CONF1_TDM_CHAN_BITS_SHIFT: u32 = 24;

/// `*_CLKM_CONF.clkm_div_num`, bits [7:0].
const CLKM_DIV_NUM_MASK: u32 = 0xFF;
/// `*_CLKM_CONF.clk_sel`, bits [28:27].
const CLKM_CLK_SEL_SHIFT: u32 = 27;
/// `*_CLKM_CONF.clk_active`, bit 26 (`i2s_ll_tx_enable_clock`). Until it is set the block has no
/// sample rate (`fs_hz` 0).
const CLKM_CLK_ACTIVE: u32 = 1 << 26;

/// `*_CLKM_DIV_CONF.div_z`, bits [8:0].
const DIV_Z_SHIFT: u32 = 0;
/// `*_CLKM_DIV_CONF.div_y`, bits [17:9].
const DIV_Y_SHIFT: u32 = 9;
/// `*_CLKM_DIV_CONF.div_x`, bits [26:18].
const DIV_X_SHIFT: u32 = 18;
/// `*_CLKM_DIV_CONF.div_yn1`, bit 27.
const DIV_YN1: u32 = 1 << 27;
/// Width of the `div_x`, `div_y` and `div_z` fields.
const DIV_FIELD_MASK: u32 = 0x1FF;

/// `*_TDM_CTRL.tot_chan_num`, bits [19:16].
const TDM_TOT_CHAN_SHIFT: u32 = 16;
/// `*_TDM_CTRL.chanN_en`, bits [15:0].
const TDM_CHAN_EN_MASK: u32 = 0xFFFF;

/// Sample width this block decodes, in bits (little-endian signed 16-bit). [`Model::frame_bytes`]
/// is the one place that compares `bits_mod` against it.
pub const PCM_BITS: u8 = 16;

/// XTAL, `clk_sel` 0.
const XTAL_HZ: u64 = 40_000_000;
/// PLL_F160M, `clk_sel` 2, which is `I2S_CLK_SRC_DEFAULT` on the C3.
const PLL_F160M_HZ: u64 = 160_000_000;

/// What an I2S period needs from the GDMA descriptor ring. `wiring::i2s` implements it; the tests
/// use a stub.
pub trait I2sDma {
    /// Length in bytes of the descriptor the ring is on in `dir`, or `None` when the pair is not
    /// started (`OUT_PERI_SEL == 3` and the link started).
    fn period_bytes(&mut self, dir: Dir) -> Option<u32>;

    /// Takes the bytes of the current TX descriptor, raises `OUT_EOF` and advances the ring.
    fn take_tx(&mut self, out: &mut Vec<u8>);

    /// Writes `bytes` into the current RX descriptor, raises `IN_SUC_EOF` and advances the ring.
    fn put_rx(&mut self, bytes: &[u8]);
}

/// Pacing state of one direction. Period *n* ends at `frame_time(base, frames, fs)`, so the
/// spacing never drifts however many periods run.
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
struct Pace {
    /// `*_CONF.start` is set.
    running: bool,
    /// A period event fired and its buffer has not moved yet.
    due: bool,
    base: VTime,
    /// Frames elapsed since `base`.
    frames: u64,
    /// The pending period event, if any. Serialized so a restored model can cancel it.
    handle: Option<EventHandle>,
}

crate::regs::store_serde!();

#[derive(Serialize, Deserialize)]
#[serde(crate = "pemu_core::serde")]
pub struct Model {
    #[serde(with = "regs_serde")]
    regs: RegStore<REG_COUNT>,
    tx: Pace,
    rx: Pace,
    touched: u64,
}

impl Default for Model {
    fn default() -> Self {
        Model {
            regs: RegStore::new(&REGS),
            tx: Pace::default(),
            rx: Pace::default(),
            touched: 0,
        }
    }
}

/// The module clock of one direction in hertz, as a fraction so nothing rounds before the sample
/// rate is taken.
///
/// ```text
/// denom = (x + 1) * z + y      numer = yn1 ? denom - z : z      (z == 0 means integer)
/// mclk  = src / (div_num + numer / denom) = src * denom / (div_num * denom + numer)
/// ```
fn mclk_ratio(clkm_conf: u32, div_conf: u32) -> Option<(u64, u64)> {
    if clkm_conf & CLKM_CLK_ACTIVE == 0 {
        return None;
    }
    let src = match (clkm_conf >> CLKM_CLK_SEL_SHIFT) & 0x3 {
        0 => XTAL_HZ,
        2 => PLL_F160M_HZ,
        // 1 is undocumented and 3 is an external clock this board does not wire.
        _ => return None,
    };
    let div_num = u64::from(clkm_conf & CLKM_DIV_NUM_MASK);
    let x = u64::from((div_conf >> DIV_X_SHIFT) & DIV_FIELD_MASK);
    let y = u64::from((div_conf >> DIV_Y_SHIFT) & DIV_FIELD_MASK);
    let z = u64::from((div_conf >> DIV_Z_SHIFT) & DIV_FIELD_MASK);
    let yn1 = div_conf & DIV_YN1 != 0;
    let (denom, numer) = if z == 0 {
        (1, 0)
    } else {
        let denom = (x + 1) * z + y;
        (denom, if yn1 { denom - z } else { z })
    };
    let divisor = div_num * denom + numer;
    (divisor != 0).then_some((src * denom, divisor))
}

impl Model {
    /// The `CONF`, `CONF1`, `CLKM_CONF`, `CLKM_DIV_CONF` and `TDM_CTRL` indices of one direction.
    const fn regs_of(dir: Dir) -> (usize, usize, usize, usize, usize) {
        match dir {
            Dir::Tx => (
                idx::I2S_TX_CONF,
                idx::I2S_TX_CONF1,
                idx::I2S_TX_CLKM_CONF,
                idx::I2S_TX_CLKM_DIV_CONF,
                idx::I2S_TX_TDM_CTRL,
            ),
            Dir::Rx => (
                idx::I2S_RX_CONF,
                idx::I2S_RX_CONF1,
                idx::I2S_RX_CLKM_CONF,
                idx::I2S_RX_CLKM_DIV_CONF,
                idx::I2S_RX_TDM_CTRL,
            ),
        }
    }

    /// Which direction's clock registers feed `dir`. In full duplex the RX channel is a slave
    /// with `sig_loopback` and shares the TX clocks, which is what `bsp_audio.c` builds.
    fn clock_dir(&self, dir: Dir) -> Dir {
        if dir == Dir::Rx
            && (self.regs.get(idx::I2S_RX_CONF) & RX_CONF_SLAVE_MOD != 0
                || self.regs.get(idx::I2S_TX_CONF) & TX_CONF_SIG_LOOPBACK != 0)
        {
            Dir::Tx
        } else {
            dir
        }
    }

    /// The sample rate of `dir` in hertz, 0 while the clock registers give none.
    ///
    /// ```text
    /// fs = mclk / (bck_div_num + 1) / ((tot_chan_num + 1) * (tdm_chan_bits + 1))
    /// ```
    ///
    /// Check: 160e6 / 39.0625 / 8 / (2 * 16) = 16000.
    pub fn fs_hz(&self, dir: Dir) -> u32 {
        let source = self.clock_dir(dir);
        let (_, conf1_i, clkm_i, div_i, tdm_i) = Self::regs_of(source);
        let Some((num, den)) = mclk_ratio(self.regs.get(clkm_i), self.regs.get(div_i)) else {
            return 0;
        };
        let conf1 = self.regs.get(conf1_i);
        let bck = u64::from((conf1 >> CONF1_BCK_DIV_SHIFT) & 0x3F) + 1;
        let chan_bits = u64::from((conf1 >> CONF1_TDM_CHAN_BITS_SHIFT) & 0x1F) + 1;
        let tot_chan = u64::from((self.regs.get(tdm_i) >> TDM_TOT_CHAN_SHIFT) & 0xF) + 1;
        let divisor = den * bck * chan_bits * tot_chan;
        if divisor == 0 {
            return 0;
        }
        u32::try_from(num / divisor).unwrap_or(u32::MAX)
    }

    /// The frame layout of `dir` in the DMA buffer. Slots per frame are the enabled channels
    /// inside `tot_chan_num + 1`; `rx_mono` makes it one. Bits per sample are `bits_mod + 1`.
    pub fn format(&self, dir: Dir) -> PcmFormat {
        let (_, conf1_i, _, _, tdm_i) = Self::regs_of(dir);
        let conf1 = self.regs.get(conf1_i);
        let tdm = self.regs.get(tdm_i);
        let tot = ((tdm >> TDM_TOT_CHAN_SHIFT) & 0xF) + 1;
        let mask = TDM_CHAN_EN_MASK & ((1u32 << tot) - 1);
        let mut slots = (tdm & mask).count_ones() as u8;
        if dir == Dir::Rx && self.regs.get(idx::I2S_RX_CONF) & RX_CONF_MONO != 0 {
            slots = 1;
        }
        PcmFormat {
            fs_hz: self.fs_hz(dir),
            bits: (((conf1 >> CONF1_BITS_MOD_SHIFT) & 0x1F) + 1) as u8,
            slots,
        }
    }

    /// Bytes one frame of `dir` occupies in the DMA buffer, or `None` for a width this path does
    /// not decode. Only 16-bit is decoded, so `wiring::i2s` cannot pace a stream it would then
    /// refuse to move. UNVERIFIED: the DMA layout of a 24-bit slot (a 32-bit word on this
    /// hardware, not three bytes).
    pub fn frame_bytes(&self, dir: Dir) -> Option<u32> {
        let fmt = self.format(dir);
        (fmt.bits == PCM_BITS).then(|| u32::from(fmt.slots) * u32::from(fmt.bits).div_ceil(8))
    }

    /// `RXEOF_NUM` in bytes: how much PCM one RX period writes (`i2s_ll.h:487`). It resets to 0x40
    /// (`i2s_reg.h:1005`), so an unprogrammed stream moves 64-byte periods, as the hardware does.
    /// The caller takes the minimum of this and the descriptor length.
    pub fn rx_eof_bytes(&self) -> u32 {
        self.regs.get(idx::I2S_RXEOF_NUM) & 0xFFF
    }

    /// Virtual time of the first frame of a period of `frames` frames that has just ended.
    pub fn period_start(&self, dir: Dir, frames: u64) -> VTime {
        let pace = self.pace(dir);
        frame_time(
            pace.base,
            pace.frames.saturating_sub(frames),
            self.fs_hz(dir),
        )
    }

    pub fn running(&self, dir: Dir) -> bool {
        self.pace(dir).running
    }

    /// Takes the "a period fell due" flag of `dir`. The caller moves one descriptor when true.
    pub fn take_due(&mut self, dir: Dir) -> bool {
        core::mem::take(&mut self.pace_mut(dir).due)
    }

    pub fn base(&self, dir: Dir) -> VTime {
        self.pace(dir).base
    }

    /// Frames of `dir` that have been paced since [`Model::base`].
    pub fn frames_done(&self, dir: Dir) -> u64 {
        self.pace(dir).frames
    }

    fn pace(&self, dir: Dir) -> &Pace {
        match dir {
            Dir::Tx => &self.tx,
            Dir::Rx => &self.rx,
        }
    }

    fn pace_mut(&mut self, dir: Dir) -> &mut Pace {
        match dir {
            Dir::Tx => &mut self.tx,
            Dir::Rx => &mut self.rx,
        }
    }

    /// Schedules the end of the next period of `dir`, `frames` frames after the last one, at
    /// `frame_time(base, frames_done + frames, fs)`. A stopped direction, no sample rate or zero
    /// frames schedules nothing.
    pub fn arm(&mut self, dir: Dir, frames: u64, now: VTime, sched: &mut Scheduler) {
        let fs = self.fs_hz(dir);
        let pace = self.pace_mut(dir);
        if let Some(handle) = pace.handle.take() {
            sched.cancel(handle);
        }
        if !pace.running || fs == 0 || frames == 0 {
            return;
        }
        let at = frame_time(pace.base, pace.frames + frames, fs);
        pace.frames += frames;
        pace.handle = Some(sched.schedule(
            now,
            at,
            EventKey {
                owner: Owner::Periph(<Self as Peripheral>::ID),
                tag: dir.tag(),
            },
        ));
    }

    /// A pacing event of `dir` fired: one descriptor has to move. An event for a direction that
    /// is not started moves nothing, so a stale event (including a restored one nothing can
    /// cancel) is harmless.
    pub fn period_due(&mut self, dir: Dir) -> Wiring {
        let pace = self.pace_mut(dir);
        pace.handle = None;
        if !pace.running {
            return Wiring::None;
        }
        pace.due = true;
        Wiring::I2sPeriod(dir)
    }

    pub fn pending(&self, dir: Dir) -> Option<EventHandle> {
        self.pace(dir).handle
    }

    /// A scheduled event of this block fired: `tag` names the direction.
    pub fn event(&mut self, tag: u16) -> Wiring {
        match Dir::of_tag(tag) {
            Some(dir) => self.period_due(dir),
            None => Wiring::None,
        }
    }

    /// Cancels the pending period of `dir`, which a stop or a reset does.
    pub fn disarm(&mut self, dir: Dir, sched: &mut Scheduler) {
        if let Some(handle) = self.pace_mut(dir).handle.take() {
            sched.cancel(handle);
        }
    }

    /// Until when a read of `off` keeps returning the same value, for the hang detector. The
    /// update bit clears inside the access and the rest is configuration, so only an access or a
    /// pacing event changes `TX_CONF` and `RX_CONF`.
    pub fn stability(&self, off: u32) -> Stability {
        match regs::reg_at(&REGS, off) {
            Some((i, _)) if REGS[i].stable_read => Stability::UntilNextEvent,
            _ => Stability::Never,
        }
    }
}

impl Model {
    /// `INT_ST` is `INT_RAW & INT_ENA`, and `STATE.tx_idle` follows `TX_CONF.tx_start`.
    fn refresh_status(&mut self) {
        let st = self.regs.get(idx::I2S_INT_RAW) & self.regs.get(idx::I2S_INT_ENA);
        self.regs.set(idx::I2S_INT_ST, st);
        let idle = if self.tx.running { 0 } else { STATE_TX_IDLE };
        self.regs.set(idx::I2S_STATE, idle);
    }

    pub fn regs(&self) -> &RegStore<REG_COUNT> {
        &self.regs
    }

    pub fn reg(&self, off: u32) -> u32 {
        regs::reg_at(&REGS, off).map_or(0, |(i, _)| self.regs.get(i))
    }

    /// Restores the registers and stops both directions on every reset that reaches the blocks.
    /// The pending event is left to `wiring::i2s`, which holds the scheduler.
    pub fn reset_regs(&mut self, kind: ResetKind) {
        if !kind.fanout.reaches_all_blocks() {
            return;
        }
        self.regs.reset(kind.scope);
        self.tx = Pace::default();
        self.rx = Pace::default();
        self.refresh_status();
    }

    pub fn load(&mut self, off: u32, size: Size, now: VTime, ledger: &mut FidelityLedger) -> u32 {
        let at = TouchAt {
            periph: Self::ID,
            size,
            now,
        };
        let Some((i, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Read, at, ledger);
            return 0;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, i, TouchAccess::Read, at, ledger);
        self.regs.read(i, byte_off, size)
    }

    /// Writes the low `size` bytes of `val` at block offset `off`. The update bits clear inside
    /// the access, and a change of `*_CONF.start` returns `Wiring::I2sPeriod` so `wiring::i2s`
    /// arms or disarms the direction.
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
        let Some((i, byte_off)) = regs::reg_at(&REGS, off) else {
            regs::hole_classed(off, TouchAccess::Write, at, ledger);
            return Wiring::None;
        };
        let touched = std::slice::from_mut(&mut self.touched);
        regs::touch_classed(touched, &REGS, i, TouchAccess::Write, at, ledger);
        let delta = self.regs.write(i, byte_off, size, val);
        let mut wiring = Wiring::None;
        match i {
            idx::I2S_TX_CONF | idx::I2S_RX_CONF => {
                let dir = if i == idx::I2S_TX_CONF {
                    Dir::Tx
                } else {
                    Dir::Rx
                };
                if delta.after & CONF_UPDATE != 0 {
                    // Every value this model uses is read at use, so the configuration is already
                    // in effect; the bit only has to read back 0.
                    let cleared = self.regs.get(i) & !CONF_UPDATE;
                    self.regs.set(i, cleared);
                }
                if delta.after & CONF_RESET != 0 {
                    // The resets are write-only and already read 0. The ring pointer and the frame
                    // clock go back to the start.
                    let pace = self.pace_mut(dir);
                    pace.frames = 0;
                    pace.base = now;
                    pace.due = false;
                }
                let running = delta.after & CONF_START != 0;
                if running != self.pace(dir).running {
                    let pace = self.pace_mut(dir);
                    pace.running = running;
                    pace.due = false;
                    if running {
                        pace.base = now;
                        pace.frames = 0;
                    }
                    wiring = Wiring::I2sPeriod(dir);
                }
            }
            idx::I2S_INT_CLR => {
                // Write-only fields: `delta.after` is the bits written 1, each clearing its
                // `INT_RAW` bit.
                let raw = self.regs.get(idx::I2S_INT_RAW) & !delta.after;
                self.regs.set(idx::I2S_INT_RAW, raw);
                self.regs.set(idx::I2S_INT_CLR, 0);
            }
            _ => {}
        }
        self.refresh_status();
        wiring
    }
}

impl Peripheral for Model {
    const ID: PeriphId = <block::I2s0 as Block>::ID;
    const BASE: u32 = <block::I2s0 as Block>::BASE;
    const SIZE: u32 = BLOCK_SIZE;

    fn reset(&mut self, kind: ResetKind, _cx: &mut Cx) {
        self.reset_regs(kind);
    }

    fn read(&mut self, off: u32, size: Size, cx: &mut Cx) -> RegRead {
        RegRead {
            val: self.load(off, size, cx.now, cx.ledger),
            stop: false,
        }
    }

    fn write(&mut self, off: u32, size: Size, val: u32, cx: &mut Cx) -> RegWrite {
        let wiring = self.store(off, size, val, cx.now, cx.ledger);
        RegWrite {
            stop: !matches!(wiring, Wiring::None),
            wiring,
        }
    }

    /// [`Model::event`]: one descriptor moves when `wiring::i2s` handles the returned wiring.
    fn on_event(&mut self, tag: u16, _cx: &mut Cx) -> Wiring {
        self.event(tag)
    }

    fn stable_until(&self, off: u32, _cx: &Cx) -> Stability {
        self.stability(off)
    }

    fn fidelity(&self, off: u32) -> Fidelity {
        regs::reg_at(&REGS, off).map_or(Fidelity::U, |(i, _)| REGS[i].class)
    }
}

#[cfg(test)]
mod tests {
    use pemu_core::reset::{ResetCause, ResetFanout, ResetScope};

    use super::*;

    const INT_RAW: u32 = 0x00C;
    const INT_ENA: u32 = 0x014;
    const INT_CLR: u32 = 0x018;
    const RX_CONF: u32 = 0x020;
    const TX_CONF: u32 = 0x024;
    const RX_CONF1: u32 = 0x028;
    const TX_CONF1: u32 = 0x02C;
    const RX_CLKM_CONF: u32 = 0x030;
    const TX_CLKM_CONF: u32 = 0x034;
    const RX_CLKM_DIV_CONF: u32 = 0x038;
    const TX_CLKM_DIV_CONF: u32 = 0x03C;
    const RX_TDM_CTRL: u32 = 0x050;
    const TX_TDM_CTRL: u32 = 0x054;
    const RXEOF_NUM: u32 = 0x064;
    const STATE: u32 = 0x06C;

    const T: VTime = VTime(0);

    /// `TX_CONF1` as the BSP programs it: `tdm_ws_width` 15, `bck_div_num` 7, `bits_mod` 15,
    /// `half_sample_bits` 15, `tdm_chan_bits` 15, `msb_shift` 1.
    const CONF1_BSP: u32 = 15 | (7 << 7) | (15 << 13) | (15 << 18) | (15 << 24) | (1 << 29);
    /// `TX_TDM_CTRL` stereo: `tot_chan_num` 1 with channels 0 and 1 enabled.
    const TDM_STEREO: u32 = 0b11 | (1 << TDM_TOT_CHAN_SHIFT);
    /// `TX_CONF` with `tx_tdm_en`, `left_align` and the BSP's other flags.
    const CONF_BSP: u32 = (1 << 19) | (1 << 15);
    /// `TX_CLKM_DIV_CONF` for 16 kHz: `div_x` 15, `div_y` 0, `div_z` 1, `div_yn1` 0.
    const DIV_16K: u32 = (15 << DIV_X_SHIFT) | (1 << DIV_Z_SHIFT);
    /// `TX_CLKM_DIV_CONF` for 8 kHz: `div_x` 7, `div_y` 0, `div_z` 1, `div_yn1` 0.
    const DIV_8K: u32 = (7 << DIV_X_SHIFT) | (1 << DIV_Z_SHIFT);
    /// `clk_sel` 2, PLL_F160M, with the module clock enabled.
    const CLK_SEL_PLL: u32 = (2 << CLKM_CLK_SEL_SHIFT) | CLKM_CLK_ACTIVE;

    /// One direction as `bsp_audio.c` configures it: stereo 16-bit at `div_num` with the
    /// fractional divider.
    fn configure(
        model: &mut Model,
        ledger: &mut FidelityLedger,
        dir: Dir,
        div_num: u32,
        frac: u32,
    ) {
        let (conf, conf1, clkm, div, tdm) = match dir {
            Dir::Tx => (
                TX_CONF,
                TX_CONF1,
                TX_CLKM_CONF,
                TX_CLKM_DIV_CONF,
                TX_TDM_CTRL,
            ),
            Dir::Rx => (
                RX_CONF,
                RX_CONF1,
                RX_CLKM_CONF,
                RX_CLKM_DIV_CONF,
                RX_TDM_CTRL,
            ),
        };
        model.store(clkm, Size::B4, CLK_SEL_PLL | div_num, T, ledger);
        model.store(div, Size::B4, frac, T, ledger);
        model.store(conf1, Size::B4, CONF1_BSP, T, ledger);
        model.store(tdm, Size::B4, TDM_STEREO, T, ledger);
        model.store(conf, Size::B4, CONF_BSP, T, ledger);
    }

    #[test]
    fn the_model_is_the_i2s0_row_of_the_table() {
        assert_eq!(<Model as Peripheral>::ID, super::super::id::I2S0);
        assert_eq!(<Model as Peripheral>::BASE, 0x6002_D000);
        assert_eq!(<Model as Peripheral>::SIZE, 0x1000);
        let model = Model::default();
        assert_eq!(model.fidelity(TX_CONF), Fidelity::B);
        assert_eq!(model.fidelity(RX_CONF), Fidelity::B);
    }

    #[test]
    fn the_clock_registers_resolve_to_the_board_sample_rates() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        assert_eq!(model.fs_hz(Dir::Tx), 0, "an unconfigured clock has no rate");

        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        assert_eq!(model.fs_hz(Dir::Tx), 16_000);

        configure(&mut model, &mut ledger, Dir::Tx, 78, DIV_8K);
        assert_eq!(model.fs_hz(Dir::Tx), 8_000);
    }

    /// `clk_sel` 0 is XTAL; `div_z` 0 means an integer divider.
    #[test]
    fn an_integer_divider_and_the_xtal_source_are_decoded() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        // z = 0: the fraction disappears and MCLK is 160e6 / 39.
        model.store(TX_CLKM_DIV_CONF, Size::B4, 0, T, &mut ledger);
        assert_eq!(model.fs_hz(Dir::Tx), 160_000_000 / 39 / 8 / 32);
        // XTAL at the same divider.
        model.store(TX_CLKM_CONF, Size::B4, CLKM_CLK_ACTIVE | 39, T, &mut ledger);
        assert_eq!(model.fs_hz(Dir::Tx), 40_000_000 / 39 / 8 / 32);
        // An external clock this board does not wire gives no rate.
        model.store(
            TX_CLKM_CONF,
            Size::B4,
            (3 << CLKM_CLK_SEL_SHIFT) | CLKM_CLK_ACTIVE | 39,
            T,
            &mut ledger,
        );
        assert_eq!(model.fs_hz(Dir::Tx), 0);
        // A divider whose module clock is not enabled gives no rate.
        model.store(
            TX_CLKM_CONF,
            Size::B4,
            CLK_SEL_PLL & !CLKM_CLK_ACTIVE | 39,
            T,
            &mut ledger,
        );
        assert_eq!(model.fs_hz(Dir::Tx), 0);
    }

    /// Full duplex: the RX sample rate comes from the TX clock registers.
    #[test]
    fn a_slave_rx_channel_takes_the_tx_sample_rate() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        configure(&mut model, &mut ledger, Dir::Rx, 78, DIV_8K);
        assert_eq!(model.fs_hz(Dir::Rx), 8_000, "its own registers, standalone");

        model.store(
            RX_CONF,
            Size::B4,
            CONF_BSP | RX_CONF_SLAVE_MOD,
            T,
            &mut ledger,
        );
        assert_eq!(model.fs_hz(Dir::Rx), 16_000, "slave: the TX clock");
        assert_eq!(model.fs_hz(Dir::Tx), 16_000);
    }

    /// The update bits read back 0 in the access that wrote them.
    #[test]
    fn the_update_bits_clear_inside_the_access_that_sets_them() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_UPDATE, T, &mut ledger);
        assert_eq!(
            model.load(TX_CONF, Size::B4, T, &mut ledger) & CONF_UPDATE,
            0,
            "i2s_ll_tx_update polls this bit",
        );
        assert_eq!(model.fs_hz(Dir::Tx), 16_000);

        configure(&mut model, &mut ledger, Dir::Rx, 39, DIV_16K);
        model.store(RX_CONF, Size::B4, CONF_BSP | CONF_UPDATE, T, &mut ledger);
        assert_eq!(
            model.load(RX_CONF, Size::B4, T, &mut ledger) & CONF_UPDATE,
            0,
            "i2s_ll_rx_update polls this bit",
        );
        // The rest of the register keeps what was written, which is what the driver reads back.
        assert_eq!(
            model.load(RX_CONF, Size::B4, T, &mut ledger) & CONF_BSP,
            CONF_BSP
        );
    }

    /// One slot per frame on an RX side with `rx_mono` set, which is what
    /// `bsp_audio_set_format(16000, 16, 1)` configures.
    #[test]
    fn the_frame_layout_follows_the_tdm_mask_and_rx_mono() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        configure(&mut model, &mut ledger, Dir::Rx, 39, DIV_16K);
        assert_eq!(model.format(Dir::Tx), PcmFormat::BSP);
        assert_eq!(model.frame_bytes(Dir::Tx), Some(4));

        // Mono TX: `tot_chan_num` 0 with channel 0 alone.
        model.store(TX_TDM_CTRL, Size::B4, 0b1, T, &mut ledger);
        assert_eq!(model.format(Dir::Tx).slots, 1);
        assert_eq!(model.frame_bytes(Dir::Tx), Some(2));

        // Mono RX: `rx_mono` makes it one slot per frame whatever the mask says.
        model.store(RX_CONF, Size::B4, CONF_BSP | RX_CONF_MONO, T, &mut ledger);
        assert_eq!(model.format(Dir::Rx).slots, 1);
        assert_eq!(model.format(Dir::Rx).fs_hz, 16_000);
        assert_eq!(model.frame_bytes(Dir::Rx), Some(2));
    }

    /// The pacing events are placed on the frame clock from the start time, so a long stream does
    /// not drift.
    #[test]
    fn the_pacing_events_are_spaced_exactly_one_buffer_apart() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);

        let wiring = model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);
        assert!(matches!(wiring, Wiring::I2sPeriod(Dir::Tx)));
        assert!(model.running(Dir::Tx));
        assert_eq!(
            model.reg(STATE) & STATE_TX_IDLE,
            0,
            "tx_idle drops while the channel runs",
        );

        // `dma_frame_num` 240: one period is 240 * 1e12 / 16000 = 15 ms.
        let period_ps = 240 * 1_000_000_000_000 / 16_000;
        assert_eq!(period_ps, 15_000_000_000);
        let mut now = T;
        let mut times = Vec::new();
        for _ in 0..64 {
            model.arm(Dir::Tx, 240, now, &mut sched);
            let at = sched.next_time().expect("a period is scheduled");
            times.push(at);
            now = at;
            let key = sched.pop_due(now).expect("the period comes due");
            assert_eq!(key.tag, Dir::Tx.tag());
        }
        for (n, at) in times.iter().enumerate() {
            assert_eq!(
                at.0,
                (n as u64 + 1) * period_ps,
                "period {n} is one buffer after the last",
            );
        }
    }

    /// A stop cancels the pending period; a restart puts the frame clock at the new start time.
    #[test]
    fn stopping_a_direction_cancels_its_period_and_a_restart_rebases_the_clock() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);
        model.arm(Dir::Tx, 240, T, &mut sched);
        assert!(sched.next_time().is_some());

        let wiring = model.store(TX_CONF, Size::B4, CONF_BSP, T, &mut ledger);
        assert!(matches!(wiring, Wiring::I2sPeriod(Dir::Tx)));
        assert!(!model.running(Dir::Tx));
        model.disarm(Dir::Tx, &mut sched);
        assert_eq!(sched.next_time(), None);
        assert_eq!(model.reg(STATE) & STATE_TX_IDLE, STATE_TX_IDLE);

        let restart = VTime::from_ms(100);
        model.store(
            TX_CONF,
            Size::B4,
            CONF_BSP | CONF_START,
            restart,
            &mut ledger,
        );
        assert_eq!(model.base(Dir::Tx), restart);
        assert_eq!(model.frames_done(Dir::Tx), 0);
        model.arm(Dir::Tx, 240, restart, &mut sched);
        assert_eq!(
            sched.next_time(),
            Some(VTime(restart.0 + 15_000_000_000)),
            "the first period after a restart is one buffer later",
        );
    }

    #[test]
    fn a_direction_without_a_sample_rate_schedules_nothing() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);
        assert!(model.running(Dir::Tx));
        assert_eq!(model.fs_hz(Dir::Tx), 0);
        model.arm(Dir::Tx, 240, T, &mut sched);
        assert_eq!(sched.next_time(), None);
    }

    #[test]
    fn an_event_marks_its_direction_due_once() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);

        assert!(!model.take_due(Dir::Tx));
        assert!(matches!(
            model.event(Dir::Tx.tag()),
            Wiring::I2sPeriod(Dir::Tx)
        ));
        assert!(model.take_due(Dir::Tx));
        assert!(!model.take_due(Dir::Tx), "the flag is taken, not left set");

        assert!(matches!(model.event(7), Wiring::None));
        assert!(!model.take_due(Dir::Tx));
        assert!(!model.take_due(Dir::Rx));
        assert_eq!(Dir::of_tag(Dir::Tx.tag()), Some(Dir::Tx));
        assert_eq!(Dir::of_tag(Dir::Rx.tag()), Some(Dir::Rx));
        assert_eq!(Dir::of_tag(7), None);
    }

    /// The pending period and its handle are snapshot state; an event that survives a stop moves
    /// nothing.
    #[test]
    fn a_period_event_that_outlives_a_stop_plays_no_extra_descriptor() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        let mut sched = Scheduler::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);
        model.arm(Dir::Tx, 240, T, &mut sched);

        let handle = model.pending(Dir::Tx).expect("the period is armed");
        assert!(sched.is_pending(handle), "and the scheduler holds it");

        model.store(TX_CONF, Size::B4, CONF_BSP, T, &mut ledger);
        assert!(!model.running(Dir::Tx));
        assert_eq!(model.pending(Dir::Tx), Some(handle));
        model.disarm(Dir::Tx, &mut sched);
        assert!(!sched.is_pending(handle));
        assert_eq!(model.pending(Dir::Tx), None);

        assert!(matches!(model.event(Dir::Tx.tag()), Wiring::None));
        assert!(!model.take_due(Dir::Tx));
    }

    /// `frame_bytes` has no answer for anything but [`PCM_BITS`], so `wiring::i2s` disarms instead
    /// of pacing a stream it would then refuse to move.
    #[test]
    fn a_sample_width_the_pcm_path_does_not_decode_has_no_frame_size() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        assert_eq!(model.format(Dir::Tx).bits, PCM_BITS);
        assert_eq!(model.frame_bytes(Dir::Tx), Some(4));

        let conf1_24 = (CONF1_BSP & !(0x1F << CONF1_BITS_MOD_SHIFT)) | (23 << CONF1_BITS_MOD_SHIFT);
        model.store(TX_CONF1, Size::B4, conf1_24, T, &mut ledger);
        assert_eq!(model.format(Dir::Tx).bits, 24);
        assert_eq!(model.frame_bytes(Dir::Tx), None);
    }

    /// An RX stream started before the driver writes `RXEOF_NUM` moves 64-byte periods.
    #[test]
    fn rxeof_num_resets_to_64_and_carries_no_unprogrammed_sentinel() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        assert_eq!(model.rx_eof_bytes(), 0x40);
        model.store(RXEOF_NUM, Size::B4, 960, T, &mut ledger);
        assert_eq!(model.rx_eof_bytes(), 960);
    }

    #[test]
    fn the_busy_wait_rows_answer_the_hang_detector_with_a_stable_read() {
        let model = Model::default();
        for off in [TX_CONF, RX_CONF] {
            assert!(
                matches!(model.stability(off), Stability::UntilNextEvent),
                "{off:#05X} is a stable_read row",
            );
        }
        assert!(matches!(model.stability(RXEOF_NUM), Stability::Never));
        assert!(
            matches!(model.stability(0x0F0), Stability::Never),
            "an offset with no register row",
        );
    }

    /// The driver does not use source 20, but the register semantics still hold.
    #[test]
    fn int_status_masks_int_raw_and_int_clr_clears_it() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        model.store(INT_ENA, Size::B4, 0xF, T, &mut ledger);
        assert_eq!(model.load(INT_RAW, Size::B4, T, &mut ledger), 0);
        model.store(INT_CLR, Size::B4, 0xF, T, &mut ledger);
        assert_eq!(model.load(INT_CLR, Size::B4, T, &mut ledger), 0);
    }

    /// A `CPU0_` reset reaches no block.
    #[test]
    fn a_reset_restores_the_registers_and_stops_both_directions() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        model.store(TX_CONF, Size::B4, CONF_BSP | CONF_START, T, &mut ledger);

        model.reset_regs(ResetKind {
            cause: ResetCause(0x0C),
            scope: ResetScope::Core,
            fanout: ResetFanout::CpuAndPms,
        });
        assert!(model.running(Dir::Tx), "a CPU0_ reset reaches no block");

        model.reset_regs(ResetKind {
            cause: ResetCause(0x03),
            scope: ResetScope::Core,
            fanout: ResetFanout::AllBlocks,
        });
        assert!(!model.running(Dir::Tx));
        assert_eq!(model.fs_hz(Dir::Tx), 0);
        assert_eq!(model.reg(TX_CONF), 0xB200, "the regs_i2s0 reset value");
        assert_eq!(model.reg(STATE) & STATE_TX_IDLE, STATE_TX_IDLE);
    }

    #[test]
    fn the_register_values_round_trip_without_the_static_table() {
        let mut model = Model::default();
        let mut ledger = FidelityLedger::default();
        configure(&mut model, &mut ledger, Dir::Tx, 39, DIV_16K);
        let saved = regs::store_values(&model.regs);
        assert_eq!(saved.len(), REG_COUNT);
        assert_eq!(regs::store_values(&regs::store_from(&REGS, &saved)), saved);
        let short = regs::store_from(&REGS, &saved[..2]);
        assert_eq!(short.get(idx::I2S_TX_CONF), REGS[idx::I2S_TX_CONF].reset);
    }
}
