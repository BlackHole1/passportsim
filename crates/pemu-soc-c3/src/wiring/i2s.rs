//! `Wiring::I2sPeriod(Dir)`: one I2S period, where the block model meets the GDMA descriptor
//! ring, the codec on the board and the host audio rings (`specs/blocks/i2s0.toml`).
//!
//! One call moves a descriptor if a period fell due, then arms the next period or disarms when
//! the direction stopped or its DMA pair is not started. So one entry point both arms the first
//! period and cancels a pending one.
//!
//! Host microphone samples are drained on this side of the frozen `BoardPorts::i2s_adc`, which
//! cannot reach `HostIo`; the board stays the analog path.

use pemu_board::traits::BoardPorts;
use pemu_core::hostio::HostIo;
use pemu_core::sched::Scheduler;
use pemu_core::time::VTime;

use crate::dma::ArenaDma;
use crate::periph::gdma::{self, PERI_I2S0};
use crate::periph::i2s0::{Dir, I2sDma, Model};

/// What one [`service`] call reports to the machine.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Serviced {
    /// The direction runs at a sample width other than `i2s0::PCM_BITS`, so it moved no PCM and
    /// disarmed; the machine counts it as a fault.
    pub width_refused: bool,
}

/// Whether host microphone samples reach the guest, as the codec's capture registers decide.
/// The machine reads them from the board and hands them to [`service_with_mic`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MicPath {
    /// The codec ADC is powered up and not muted. A closed path drops the buffered host audio,
    /// as a powered-down or muted ADC loses it on the board.
    pub open: bool,
    /// `ADCDAT_SEL` (`0x44` bits 6 to 4) is not 0: only slot 0 carries the microphone and the
    /// other slots read 0. UNVERIFIED: the DAC-right loopback of value 5 is not modeled.
    pub first_slot_only: bool,
}

impl MicPath {
    /// The path [`service`] assumes: open, the microphone on every slot.
    pub const OPEN: MicPath = MicPath {
        open: true,
        first_slot_only: false,
    };
}

/// Services one period of `dir` with [`MicPath::OPEN`].
pub fn service(
    i2s: &mut Model,
    dir: Dir,
    now: VTime,
    sched: &mut Scheduler,
    dma: &mut dyn I2sDma,
    board: &mut dyn BoardPorts,
    io: &mut HostIo,
) -> Serviced {
    service_with_mic(i2s, dir, now, sched, dma, board, io, MicPath::OPEN)
}

/// [`service`] under the codec's gating of the host microphone.
///
/// Audio that arrives on RX while the direction is stopped or `mic` is closed is dropped (and
/// counted), as on silicon, rather than kept for a later period.
#[allow(clippy::too_many_arguments)]
pub fn service_with_mic(
    i2s: &mut Model,
    dir: Dir,
    now: VTime,
    sched: &mut Scheduler,
    dma: &mut dyn I2sDma,
    board: &mut dyn BoardPorts,
    io: &mut HostIo,
    mic: MicPath,
) -> Serviced {
    let width_refused = i2s.running(dir) && i2s.frame_bytes(dir).is_none();
    if dir == Dir::Rx && !(mic.open && i2s.running(dir)) {
        io.audio_in.discard();
    }
    if i2s.take_due(dir) {
        move_one(i2s, dir, now, dma, board, io, mic);
    }
    // A width `move_one` would refuse is never paced either.
    let Some(frame_bytes) = i2s.frame_bytes(dir).filter(|bytes| *bytes != 0) else {
        i2s.disarm(dir, sched);
        return Serviced { width_refused };
    };
    let Some(bytes) = period_bytes(i2s, dir, dma) else {
        i2s.disarm(dir, sched);
        return Serviced { width_refused };
    };
    i2s.arm(dir, u64::from(bytes / frame_bytes), now, sched);
    Serviced { width_refused }
}

/// Bytes one period of `dir` moves; RX takes the smaller of `RXEOF_NUM` and the descriptor size.
/// `None` means the direction is stopped or its GDMA pair is not started.
///
/// `RXEOF_NUM` resets to 0x40, so a stream started before the driver writes it moves 64-byte
/// periods, as the hardware counter does.
fn period_bytes(i2s: &Model, dir: Dir, dma: &mut dyn I2sDma) -> Option<u32> {
    if !i2s.running(dir) {
        return None;
    }
    let link = dma.period_bytes(dir)?;
    Some(match dir {
        Dir::Rx => i2s.rx_eof_bytes().min(link),
        Dir::Tx => link,
    })
}

/// Moves one descriptor of `dir`.
fn move_one(
    i2s: &mut Model,
    dir: Dir,
    now: VTime,
    dma: &mut dyn I2sDma,
    board: &mut dyn BoardPorts,
    io: &mut HostIo,
    mic: MicPath,
) {
    let fmt = i2s.format(dir);
    if fmt.fs_hz == 0 || fmt.slots == 0 || i2s.frame_bytes(dir).is_none() {
        return;
    }
    match dir {
        Dir::Tx => {
            let mut bytes = Vec::new();
            dma.take_tx(&mut bytes);
            let samples: Vec<i16> = bytes
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let frames = samples.len() as u64 / u64::from(fmt.slots);
            if frames == 0 {
                return;
            }
            board.i2s_dac(now, fmt, &samples);
            io.audio_out.write(
                i2s.period_start(dir, frames),
                fmt.fs_hz,
                u16::from(fmt.slots),
                &samples,
            );
        }
        Dir::Rx => {
            let Some(bytes) = period_bytes(i2s, dir, dma) else {
                return;
            };
            let samples = (bytes / 2) as usize;
            let frames = samples as u64 / u64::from(fmt.slots);
            if frames == 0 {
                return;
            }
            let mut pcm = vec![0i16; samples];
            // The codec fills the buffer first.
            board.i2s_adc(now, fmt, &mut pcm);
            // Host-injected samples take precedence, sample by sample; `pop_or_silence`
            // zero-pads and counts the underflow. A run the host never fed leaves the ring
            // alone. UNVERIFIED: no PGA gain applies to injected samples.
            if mic.open && io.audio_in.head() > 0 {
                let copied = io.audio_in.pop_or_silence(&mut pcm);
                let stride = usize::from(fmt.slots);
                if mic.first_slot_only && stride > 1 {
                    for frame in pcm.chunks_exact_mut(stride) {
                        frame[1..].fill(0);
                    }
                    io.audio_in.count_dropped(copied / stride * (stride - 1));
                }
            }
            let mut out = Vec::with_capacity(samples * 2);
            for sample in &pcm {
                out.extend_from_slice(&sample.to_le_bytes());
            }
            dma.put_rx(&out);
        }
    }
}

/// [`I2sDma`] over the GDMA engine and guest memory. The period is the current descriptor's
/// length (TX) or size (RX); the channel is found by `PERI_SEL`, never by pair number.
pub struct GdmaI2s<'a, 'm> {
    pub gdma: &'a mut gdma::Engine,
    pub mem: &'a mut ArenaDma<'m>,
    /// Interrupt levels the walks changed, per channel, for the machine to drive.
    pub irq: [Option<bool>; gdma::CHANNELS],
}

impl GdmaI2s<'_, '_> {
    fn channel(&self, dir: Dir) -> Option<usize> {
        match dir {
            Dir::Tx => self.gdma.tx_channel_of(PERI_I2S0),
            Dir::Rx => self.gdma.rx_channel_of(PERI_I2S0),
        }
    }
}

impl I2sDma for GdmaI2s<'_, '_> {
    fn period_bytes(&mut self, dir: Dir) -> Option<u32> {
        let ch = self.channel(dir)?;
        let (at, off) = match dir {
            Dir::Tx => self.gdma.tx_position(ch)?,
            Dir::Rx => self.gdma.rx_position(ch)?,
        };
        let d = gdma::Descriptor::read(at, self.mem);
        Some(match dir {
            Dir::Tx => d.length.saturating_sub(off),
            Dir::Rx => d.size.saturating_sub(off),
        })
    }

    fn take_tx(&mut self, out: &mut Vec<u8>) {
        let Some(bytes) = self.period_bytes(Dir::Tx) else {
            return;
        };
        let Some(ch) = self.channel(Dir::Tx) else {
            return;
        };
        let pull = self.gdma.tx_pull(ch, bytes, self.mem);
        out.extend_from_slice(&pull.bytes);
        if pull.irq.is_some() {
            self.irq[ch] = pull.irq;
        }
    }

    fn put_rx(&mut self, bytes: &[u8]) {
        let Some(ch) = self.channel(Dir::Rx) else {
            return;
        };
        let push = self.gdma.rx_push(ch, bytes, self.mem);
        if push.irq.is_some() {
            self.irq[ch] = push.irq;
        }
    }
}
