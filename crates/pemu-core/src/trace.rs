//! The canonical MMIO and IRQ trace: the same events in the same order for two runs of one
//! identity, whatever the block size, slice size or poll fast-forward did. Records carry logical
//! instruction counts, never time (a fast-forwarded iteration counts as executed), and consecutive
//! identical tracked reads fold into one [`TraceEvent::PollRun`].
//!
//! A record is a tag byte, then LEB128 varints with every repeating field a delta. The encoding is
//! the digest's input: changing it moves every committed trace digest.
//!
//! ```text
//! record  := tag:u8, dinsns:varint, fields
//! tag     := kind << 4 | low
//! kind 1 MmioRead   low = size code   fields = pc, addr, val
//! kind 2 MmioWrite  low = size code   fields = pc, addr, val
//! kind 3 PollRun    low = size code   fields = pc, addr, val, count
//! kind 4 Irq        low = IRQ variant fields = per variant (below)
//! kind 5 Reset      low = reset code  fields = cause:u8
//! size code   0 = 1 byte, 1 = 2 bytes, 2 = 4 bytes
//! reset code  bits 0-1 scope: 0 = Chip, 1 = System, 2 = Core; bit 2 set = CpuAndPms fan-out
//! IRQ variant 0 Raise (source), 1 Lower (source), 2 Take (line, pc), 3 Return (pc)
//! dinsns      instruction count minus the previous record's; records are non-decreasing in it
//! ```

use serde::{Deserialize, Serialize};

use crate::irq_source::IrqSource;
use crate::reset::{ResetCause, ResetFanout, ResetKind, ResetScope};

/// Largest record: tag, 10-byte delta, a poll run's three 5-byte fields and 10-byte count.
pub const MAX_RECORD_BYTES: usize = 1 + 10 + 3 * 5 + 10;

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum IrqEvent {
    /// A source level went high.
    Raise { source: IrqSource },
    /// A source level went low.
    Lower { source: IrqSource },
    /// The hart entered the handler of CPU line `line`; `pc` is the interrupted PC (`mepc`).
    Take { line: u8, pc: u32 },
    /// The hart left a handler with `mret`; `pc` is where it returns to.
    Return { pc: u32 },
}

/// What one trace record says happened: guest-observable behavior only.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum TraceEvent {
    /// `size` is the access width in bytes (1, 2 or 4).
    MmioRead {
        pc: u32,
        addr: u32,
        val: u32,
        size: u8,
    },
    MmioWrite {
        pc: u32,
        addr: u32,
        val: u32,
        size: u8,
    },
    /// `count` consecutive identical reads; two widths of one address are not the same read.
    PollRun {
        pc: u32,
        addr: u32,
        val: u32,
        size: u8,
        count: u64,
    },
    Irq(IrqEvent),
    /// A reset, with the cause the ROM banner prints and the scope it cleared.
    Reset(ResetKind),
}

/// One record; `insns` is the logical instruction count, non-decreasing along a trace.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TraceRecord {
    pub insns: u64,
    pub ev: TraceEvent,
}

/// Which record kinds a sink writes. Filtering changes the trace, never the run.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TraceKinds(pub u8);

impl TraceKinds {
    /// MMIO reads, and the poll runs they fold into.
    pub const MMIO_READ: TraceKinds = TraceKinds(0x1);
    /// MMIO writes, the stream the QEMU oracle comparison uses.
    pub const MMIO_WRITE: TraceKinds = TraceKinds(0x2);
    pub const IRQ: TraceKinds = TraceKinds(0x4);
    pub const RESET: TraceKinds = TraceKinds(0x8);
    pub const ALL: TraceKinds = TraceKinds(0xF);

    /// Whether this set holds every kind of `other`.
    pub const fn has(self, other: TraceKinds) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: TraceKinds) -> TraceKinds {
        TraceKinds(self.0 | other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// Size code for the tag byte; a width other than 1 or 2 records as 4, because an MMIO access is
/// word-sized unless a model narrows it.
const fn size_code(size: u8) -> u8 {
    match size {
        1 => 0,
        2 => 1,
        _ => 2,
    }
}

const fn code_size(code: u8) -> u8 {
    match code {
        0 => 1,
        1 => 2,
        _ => 4,
    }
}

/// Reset code for the tag byte: scope in bits 0-1, fan-out in bit 2. A `CPU0_` and a `CORE_`
/// reset share the scope and differ in the blocks they reach, so both belong in the record.
const fn reset_code(kind: ResetKind) -> u8 {
    let scope = match kind.scope {
        ResetScope::Chip => 0,
        ResetScope::System => 1,
        ResetScope::Core => 2,
    };
    let fanout = match kind.fanout {
        ResetFanout::AllBlocks => 0,
        ResetFanout::CpuAndPms => 4,
    };
    scope | fanout
}

const fn code_reset(code: u8) -> Option<(ResetScope, ResetFanout)> {
    let scope = match code & 0x3 {
        0 => ResetScope::Chip,
        1 => ResetScope::System,
        2 => ResetScope::Core,
        _ => return None,
    };
    let fanout = match code & 0xC {
        0 => ResetFanout::AllBlocks,
        4 => ResetFanout::CpuAndPms,
        _ => return None,
    };
    Some((scope, fanout))
}

/// Appends `v` as LEB128 and returns the new length.
fn put_varint(out: &mut [u8], mut at: usize, mut v: u64) -> usize {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        out[at] = if v == 0 { byte } else { byte | 0x80 };
        at += 1;
        if v == 0 {
            return at;
        }
    }
}

/// Reads one LEB128 value and the position after it; `None` if truncated or over 64 bits.
fn get_varint(bytes: &[u8], mut at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(at)?;
        at += 1;
        v |= u64::from(byte & 0x7F).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some((v, at));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Encodes one record and returns its length. `prev_insns` is the previous record's count (0 for
/// the first); a count that goes backwards encodes as a zero delta.
pub fn encode(rec: &TraceRecord, prev_insns: u64, out: &mut [u8; MAX_RECORD_BYTES]) -> usize {
    let (tag, at) = match rec.ev {
        TraceEvent::MmioRead { size, .. } => (0x10 | size_code(size), 1),
        TraceEvent::MmioWrite { size, .. } => (0x20 | size_code(size), 1),
        TraceEvent::PollRun { size, .. } => (0x30 | size_code(size), 1),
        TraceEvent::Irq(ev) => (
            0x40 | match ev {
                IrqEvent::Raise { .. } => 0,
                IrqEvent::Lower { .. } => 1,
                IrqEvent::Take { .. } => 2,
                IrqEvent::Return { .. } => 3,
            },
            1,
        ),
        TraceEvent::Reset(kind) => (0x50 | reset_code(kind), 1),
    };
    out[0] = tag;
    let mut at = put_varint(out, at, rec.insns.saturating_sub(prev_insns));
    match rec.ev {
        TraceEvent::MmioRead { pc, addr, val, .. }
        | TraceEvent::MmioWrite { pc, addr, val, .. } => {
            at = put_varint(out, at, u64::from(pc));
            at = put_varint(out, at, u64::from(addr));
            at = put_varint(out, at, u64::from(val));
        }
        TraceEvent::PollRun {
            pc,
            addr,
            val,
            count,
            ..
        } => {
            at = put_varint(out, at, u64::from(pc));
            at = put_varint(out, at, u64::from(addr));
            at = put_varint(out, at, u64::from(val));
            at = put_varint(out, at, count);
        }
        TraceEvent::Irq(ev) => match ev {
            IrqEvent::Raise { source } | IrqEvent::Lower { source } => {
                at = put_varint(out, at, u64::from(source.0));
            }
            IrqEvent::Take { line, pc } => {
                at = put_varint(out, at, u64::from(line));
                at = put_varint(out, at, u64::from(pc));
            }
            IrqEvent::Return { pc } => {
                at = put_varint(out, at, u64::from(pc));
            }
        },
        TraceEvent::Reset(kind) => {
            out[at] = kind.cause.0;
            at += 1;
        }
    }
    at
}

/// Decodes the record at the start of `bytes` and its length; `None` if truncated or unknown.
pub fn decode(bytes: &[u8], prev_insns: u64) -> Option<(TraceRecord, usize)> {
    let tag = *bytes.first()?;
    let (dinsns, at) = get_varint(bytes, 1)?;
    let insns = prev_insns.checked_add(dinsns)?;
    let low = tag & 0xF;
    let size = code_size(low);
    let (ev, at) = match tag >> 4 {
        1..=3 => {
            let (pc, at) = get_varint(bytes, at)?;
            let (addr, at) = get_varint(bytes, at)?;
            let (val, at) = get_varint(bytes, at)?;
            let (pc, addr, val) = (
                u32::try_from(pc).ok()?,
                u32::try_from(addr).ok()?,
                u32::try_from(val).ok()?,
            );
            match tag >> 4 {
                1 => (
                    TraceEvent::MmioRead {
                        pc,
                        addr,
                        val,
                        size,
                    },
                    at,
                ),
                2 => (
                    TraceEvent::MmioWrite {
                        pc,
                        addr,
                        val,
                        size,
                    },
                    at,
                ),
                _ => {
                    let (count, at) = get_varint(bytes, at)?;
                    (
                        TraceEvent::PollRun {
                            pc,
                            addr,
                            val,
                            size,
                            count,
                        },
                        at,
                    )
                }
            }
        }
        4 => match low {
            0 | 1 => {
                let (source, at) = get_varint(bytes, at)?;
                let source = IrqSource(u8::try_from(source).ok()?);
                let ev = if low == 0 {
                    IrqEvent::Raise { source }
                } else {
                    IrqEvent::Lower { source }
                };
                (TraceEvent::Irq(ev), at)
            }
            2 => {
                let (line, at) = get_varint(bytes, at)?;
                let (pc, at) = get_varint(bytes, at)?;
                (
                    TraceEvent::Irq(IrqEvent::Take {
                        line: u8::try_from(line).ok()?,
                        pc: u32::try_from(pc).ok()?,
                    }),
                    at,
                )
            }
            3 => {
                let (pc, at) = get_varint(bytes, at)?;
                (
                    TraceEvent::Irq(IrqEvent::Return {
                        pc: u32::try_from(pc).ok()?,
                    }),
                    at,
                )
            }
            _ => return None,
        },
        5 => {
            let (scope, fanout) = code_reset(low)?;
            let cause = ResetCause(*bytes.get(at)?);
            (
                TraceEvent::Reset(ResetKind {
                    cause,
                    scope,
                    fanout,
                }),
                at + 1,
            )
        }
        _ => return None,
    };
    Some((TraceRecord { insns, ev }, at))
}

/// Default replay window for `debug trace` and the hang detector; the digest covers every record.
pub const DEFAULT_RECENT_RECORDS: usize = 4096;

/// Trace record sink reached through `Cx::trace`; off, a call costs one predictable branch. It is
/// output, not guest state: no snapshot carries it, and a restored machine starts a fresh trace.
/// UNVERIFIED: a design choice.
#[derive(Default, Debug)]
pub struct TraceSink {
    state: Option<Box<TraceState>>,
}

#[derive(Debug)]
struct TraceState {
    kinds: TraceKinds,
    hasher: blake3::Hasher,
    /// Ring indexed by absolute record number modulo its length.
    recent: Vec<TraceRecord>,
    cap: usize,
    /// Records ever closed; the absolute cursor of the next one.
    head: u64,
    bytes: u64,
    /// Count of the last encoded record, the base of the next delta.
    prev_insns: u64,
    /// The poll run being folded, not yet in the digest.
    open: Option<OpenRun>,
}

#[derive(Copy, Clone, Debug)]
struct OpenRun {
    pc: u32,
    addr: u32,
    val: u32,
    size: u8,
    /// Count at the first read, where the busy-wait started with or without fast-forward.
    insns: u64,
    count: u64,
}

impl TraceSink {
    /// A sink that records `kinds`, keeping the last `recent` (may be 0) records for replay.
    pub fn new(kinds: TraceKinds, recent: usize) -> Self {
        TraceSink {
            state: Some(Box::new(TraceState {
                kinds,
                hasher: blake3::Hasher::new(),
                // The whole window up front, so the traced path never reallocates.
                recent: Vec::with_capacity(recent),
                cap: recent,
                head: 0,
                bytes: 0,
                prev_insns: 0,
                open: None,
            })),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.state.is_some()
    }

    pub fn kinds(&self) -> TraceKinds {
        self.state.as_ref().map_or(TraceKinds(0), |s| s.kinds)
    }

    /// Records one MMIO read, folding it into the open poll run when it repeats the last one.
    #[inline]
    pub fn mmio_read(&mut self, insns: u64, pc: u32, addr: u32, val: u32, size: u8) {
        if self.state.is_some() {
            self.read_cold(insns, pc, addr, val, size);
        }
    }

    /// Records one MMIO write, which closes any open poll run: a write ends the iteration.
    #[inline]
    pub fn mmio_write(&mut self, insns: u64, pc: u32, addr: u32, val: u32, size: u8) {
        if self.state.is_some() {
            self.event_cold(
                insns,
                TraceEvent::MmioWrite {
                    pc,
                    addr,
                    val,
                    size,
                },
                TraceKinds::MMIO_WRITE,
            );
        }
    }

    #[inline]
    pub fn irq(&mut self, insns: u64, ev: IrqEvent) {
        if self.state.is_some() {
            self.event_cold(insns, TraceEvent::Irq(ev), TraceKinds::IRQ);
        }
    }

    #[inline]
    pub fn reset(&mut self, insns: u64, kind: ResetKind) {
        if self.state.is_some() {
            self.event_cold(insns, TraceEvent::Reset(kind), TraceKinds::RESET);
        }
    }

    /// Credits `count` iterations skipped by poll fast-forward to the open run, if any. A sink
    /// holds one open run, so the machine may fast-forward only a loop with one tracked read.
    #[inline]
    pub fn poll_repeat(&mut self, count: u64) {
        if self.state.is_some() {
            self.repeat_cold(count);
        }
    }

    /// Closes the open poll run. The run loop calls it only at the end of a run: closing at a
    /// stop, snapshot or slice boundary would make the trace depend on where the run paused.
    pub fn flush(&mut self) {
        if let Some(st) = self.state.as_deref_mut() {
            st.close();
        }
    }

    /// blake3 over the whole encoded stream, the open poll run included without closing it, so
    /// reading it mid-busy-wait cannot change the trace. Off, it digests the empty stream.
    pub fn digest(&self) -> [u8; 32] {
        match self.state.as_deref() {
            Some(st) => *st.observe().0.finalize().as_bytes(),
            None => *blake3::Hasher::new().finalize().as_bytes(),
        }
    }

    pub fn head(&self) -> u64 {
        self.state.as_deref().map_or(0, |s| s.head)
    }

    /// Absolute cursor of the oldest record still in the replay window.
    pub fn tail(&self) -> u64 {
        self.state
            .as_deref()
            .map_or(0, |s| s.head - s.recent.len() as u64)
    }

    /// Encoded length of the stream [`TraceSink::digest`] covers.
    pub fn bytes(&self) -> u64 {
        self.state.as_deref().map_or(0, |s| s.observe().1)
    }

    /// The replay window, oldest first; the open poll run is only in [`TraceSink::pending`].
    pub fn records(&self) -> impl Iterator<Item = TraceRecord> + '_ {
        let st = self.state.as_deref();
        let n = st.map_or(0, |s| s.recent.len()) as u64;
        (0..n).filter_map(move |i| {
            let s = st?;
            let at = (s.head - n + i) % s.recent.len() as u64;
            Some(s.recent[at as usize])
        })
    }

    /// The open poll run, as the record it would close into.
    pub fn pending(&self) -> Option<TraceRecord> {
        self.state.as_deref()?.open.map(|o| o.record())
    }

    #[cold]
    #[inline(never)]
    fn read_cold(&mut self, insns: u64, pc: u32, addr: u32, val: u32, size: u8) {
        let Some(st) = self.state.as_deref_mut() else {
            return;
        };
        if !st.kinds.has(TraceKinds::MMIO_READ) {
            return;
        }
        if let Some(open) = st.open.as_mut()
            && open.pc == pc
            && open.addr == addr
            && open.val == val
            && open.size == size
        {
            open.count += 1;
            return;
        }
        st.close();
        st.open = Some(OpenRun {
            pc,
            addr,
            val,
            size,
            insns,
            count: 1,
        });
    }

    #[cold]
    #[inline(never)]
    fn event_cold(&mut self, insns: u64, ev: TraceEvent, kind: TraceKinds) {
        let Some(st) = self.state.as_deref_mut() else {
            return;
        };
        // Close even when the event is filtered out: it still ended the iteration, and folding
        // the reads around it would invent a busy-wait for the hang detector.
        st.close();
        if !st.kinds.has(kind) {
            return;
        }
        st.emit(TraceRecord { insns, ev });
    }

    #[cold]
    #[inline(never)]
    fn repeat_cold(&mut self, count: u64) {
        if let Some(st) = self.state.as_deref_mut()
            && let Some(open) = st.open.as_mut()
        {
            open.count = open.count.saturating_add(count);
        }
    }
}

impl OpenRun {
    fn record(self) -> TraceRecord {
        let ev = if self.count == 1 {
            TraceEvent::MmioRead {
                pc: self.pc,
                addr: self.addr,
                val: self.val,
                size: self.size,
            }
        } else {
            TraceEvent::PollRun {
                pc: self.pc,
                addr: self.addr,
                val: self.val,
                size: self.size,
                count: self.count,
            }
        };
        TraceRecord {
            insns: self.insns,
            ev,
        }
    }
}

impl TraceState {
    /// A hasher copy with the open run encoded exactly as [`TraceState::emit`] would, and the
    /// stream length. Touches neither `open` nor `prev_insns`, so reading has no side effects.
    fn observe(&self) -> (blake3::Hasher, u64) {
        let mut hasher = self.hasher.clone();
        let mut bytes = self.bytes;
        if let Some(open) = self.open {
            let rec = open.record();
            let rec = TraceRecord {
                insns: rec.insns.max(self.prev_insns),
                ev: rec.ev,
            };
            let mut buf = [0u8; MAX_RECORD_BYTES];
            let n = encode(&rec, self.prev_insns, &mut buf);
            hasher.update(&buf[..n]);
            bytes += n as u64;
        }
        (hasher, bytes)
    }

    fn close(&mut self) {
        if let Some(open) = self.open.take() {
            self.emit(open.record());
        }
    }

    fn emit(&mut self, rec: TraceRecord) {
        // Clamp, so the delta encoding and the replay window agree on an out-of-order caller.
        let rec = TraceRecord {
            insns: rec.insns.max(self.prev_insns),
            ev: rec.ev,
        };
        let mut buf = [0u8; MAX_RECORD_BYTES];
        let n = encode(&rec, self.prev_insns, &mut buf);
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        self.prev_insns = rec.insns;
        if self.cap > 0 {
            if self.recent.len() < self.cap {
                self.recent.push(rec);
            } else {
                let at = (self.head % self.cap as u64) as usize;
                self.recent[at] = rec;
            }
        }
        self.head += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `spi_device_polling_end` busy-wait: one PC reading one SPI2 register.
    const LOOP_PC: u32 = 0x4038_977c;
    const SPI2_ST: u32 = 0x6002_403C;
    const TICKS: u32 = 0x3FCA_B524;

    fn sink() -> TraceSink {
        TraceSink::new(TraceKinds::ALL, 8)
    }

    fn drive(t: &mut TraceSink, poll_ff: bool) {
        t.mmio_write(10, LOOP_PC, SPI2_ST, 0x8000_0000, 4);
        t.irq(
            11,
            IrqEvent::Raise {
                source: IrqSource(19),
            },
        );
        // 200 iterations of the busy-wait, either executed or fast-forwarded after 64.
        if poll_ff {
            for i in 0..64 {
                t.mmio_read(20 + i * 6, LOOP_PC, SPI2_ST, 0, 4);
            }
            t.poll_repeat(136);
        } else {
            for i in 0..200 {
                t.mmio_read(20 + i * 6, LOOP_PC, SPI2_ST, 0, 4);
            }
        }
        t.mmio_read(1220, LOOP_PC, SPI2_ST, 1, 4);
        t.irq(
            1221,
            IrqEvent::Take {
                line: 7,
                pc: 0x4038_0000,
            },
        );
        t.mmio_read(1230, LOOP_PC + 8, TICKS, 42, 4);
        t.irq(1240, IrqEvent::Return { pc: 0x4038_0000 });
        t.irq(
            1241,
            IrqEvent::Lower {
                source: IrqSource(19),
            },
        );
        t.reset(
            1300,
            ResetKind::of(ResetCause::USB_UART_CHIP).expect("documented"),
        );
        t.flush();
    }

    #[test]
    fn every_record_kind_round_trips_through_the_encoding() {
        let evs = [
            TraceEvent::MmioRead {
                pc: LOOP_PC,
                addr: SPI2_ST,
                val: 0xDEAD_BEEF,
                size: 4,
            },
            TraceEvent::MmioRead {
                pc: 0,
                addr: 0,
                val: 0,
                size: 1,
            },
            TraceEvent::MmioWrite {
                pc: LOOP_PC,
                addr: SPI2_ST,
                val: 0x8000_0000,
                size: 2,
            },
            TraceEvent::PollRun {
                pc: LOOP_PC,
                addr: SPI2_ST,
                val: 0,
                size: 4,
                count: u64::MAX,
            },
            TraceEvent::Irq(IrqEvent::Raise {
                source: IrqSource(61),
            }),
            TraceEvent::Irq(IrqEvent::Lower {
                source: IrqSource(0),
            }),
            TraceEvent::Irq(IrqEvent::Take {
                line: 31,
                pc: 0xFFFF_FFFF,
            }),
            TraceEvent::Irq(IrqEvent::Return { pc: 0x4004_7E9E }),
            TraceEvent::Reset(ResetKind {
                cause: ResetCause::POWERON,
                scope: ResetScope::Chip,
                fanout: ResetFanout::AllBlocks,
            }),
            TraceEvent::Reset(ResetKind {
                cause: ResetCause(0x15),
                scope: ResetScope::System,
                fanout: ResetFanout::AllBlocks,
            }),
            TraceEvent::Reset(ResetKind {
                cause: ResetCause::RTC_SW_CPU,
                scope: ResetScope::Core,
                fanout: ResetFanout::CpuAndPms,
            }),
        ];
        for ev in evs {
            for (prev, insns) in [(0u64, 0u64), (7, 7), (100, 1_000_000), (0, u64::MAX)] {
                let rec = TraceRecord { insns, ev };
                let mut buf = [0u8; MAX_RECORD_BYTES];
                let n = encode(&rec, prev, &mut buf);
                assert!(n <= MAX_RECORD_BYTES, "{rec:?} encoded to {n} bytes");
                let (back, used) = decode(&buf[..n], prev).expect("decodes");
                assert_eq!(used, n, "{rec:?}");
                assert_eq!(back, rec, "{rec:?}");
            }
        }
    }

    #[test]
    fn decode_refuses_a_truncated_or_unknown_record() {
        let rec = TraceRecord {
            insns: 1_000_000,
            ev: TraceEvent::MmioRead {
                pc: LOOP_PC,
                addr: SPI2_ST,
                val: 7,
                size: 4,
            },
        };
        let mut buf = [0u8; MAX_RECORD_BYTES];
        let n = encode(&rec, 0, &mut buf);
        for cut in 0..n {
            assert!(
                decode(&buf[..cut], 0).is_none(),
                "{cut} of {n} bytes decoded"
            );
        }
        assert!(decode(&[0x00, 0x00], 0).is_none(), "kind 0 is not a record");
        assert!(
            decode(&[0x64, 0x00], 0).is_none(),
            "IRQ variant 4 is not a record"
        );
        assert!(
            decode(&[0x53, 0x00, 0x01], 0).is_none(),
            "scope code 3 is not a scope"
        );
    }

    #[test]
    fn two_identical_runs_produce_the_same_trace() {
        let (mut a, mut b) = (sink(), sink());
        drive(&mut a, false);
        drive(&mut b, false);
        assert_eq!(a.digest(), b.digest());
        assert_eq!(a.head(), b.head());
        assert_eq!(a.bytes(), b.bytes());
        assert_eq!(
            a.records().collect::<Vec<_>>(),
            b.records().collect::<Vec<_>>()
        );
        let mut c = sink();
        drive(&mut c, false);
        c.mmio_read(1400, LOOP_PC, SPI2_ST, 2, 4);
        assert_ne!(a.digest(), c.digest());
    }

    #[test]
    fn the_trace_is_the_same_with_and_without_poll_fast_forward() {
        let (mut off, mut on) = (sink(), sink());
        drive(&mut off, false);
        drive(&mut on, true);
        assert_eq!(off.digest(), on.digest());
        assert_eq!(off.head(), on.head());
        assert_eq!(
            off.records().collect::<Vec<_>>(),
            on.records().collect::<Vec<_>>()
        );
        let run = off
            .records()
            .find(|r| matches!(r.ev, TraceEvent::PollRun { .. }))
            .expect("the busy-wait folded");
        assert_eq!(run.insns, 20, "the run is stamped where it started");
        assert_eq!(
            run.ev,
            TraceEvent::PollRun {
                pc: LOOP_PC,
                addr: SPI2_ST,
                val: 0,
                size: 4,
                count: 200
            }
        );
    }

    #[test]
    fn only_identical_consecutive_reads_fold() {
        let mut t = TraceSink::new(TraceKinds::ALL, 64);
        t.mmio_read(1, LOOP_PC, SPI2_ST, 0, 4);
        t.mmio_read(2, LOOP_PC, SPI2_ST, 0, 4);
        t.mmio_read(3, LOOP_PC, SPI2_ST, 1, 4); // another value: a new run
        t.mmio_read(4, LOOP_PC, SPI2_ST, 1, 2); // another width: a new run
        t.mmio_read(5, LOOP_PC + 4, SPI2_ST, 1, 2); // another PC: a new run
        t.mmio_read(6, LOOP_PC + 4, TICKS, 1, 2); // another address: a new run
        t.flush();
        let kinds: Vec<_> = t.records().map(|r| r.ev).collect();
        assert_eq!(
            kinds,
            vec![
                TraceEvent::PollRun {
                    pc: LOOP_PC,
                    addr: SPI2_ST,
                    val: 0,
                    size: 4,
                    count: 2
                },
                TraceEvent::MmioRead {
                    pc: LOOP_PC,
                    addr: SPI2_ST,
                    val: 1,
                    size: 4
                },
                TraceEvent::MmioRead {
                    pc: LOOP_PC,
                    addr: SPI2_ST,
                    val: 1,
                    size: 2
                },
                TraceEvent::MmioRead {
                    pc: LOOP_PC + 4,
                    addr: SPI2_ST,
                    val: 1,
                    size: 2
                },
                TraceEvent::MmioRead {
                    pc: LOOP_PC + 4,
                    addr: TICKS,
                    val: 1,
                    size: 2
                },
            ]
        );
    }

    #[test]
    fn a_write_an_irq_or_a_reset_closes_the_open_run() {
        type Closer = fn(&mut TraceSink);
        let closers: [(&str, Closer); 3] = [
            ("write", |t| t.mmio_write(3, LOOP_PC, SPI2_ST, 1, 4)),
            ("irq", |t| {
                t.irq(
                    3,
                    IrqEvent::Raise {
                        source: IrqSource(19),
                    },
                )
            }),
            ("reset", |t| {
                t.reset(3, ResetKind::of(ResetCause::POWERON).expect("documented"))
            }),
        ];
        for (name, close) in closers {
            let mut t = TraceSink::new(TraceKinds::ALL, 64);
            t.mmio_read(1, LOOP_PC, SPI2_ST, 0, 4);
            t.mmio_read(2, LOOP_PC, SPI2_ST, 0, 4);
            assert_eq!(t.head(), 0, "{name}: the run is still open");
            close(&mut t);
            assert_eq!(t.head(), 2, "{name}: the run closed before the event");
            t.mmio_read(4, LOOP_PC, SPI2_ST, 0, 4);
            t.flush();
            let evs: Vec<_> = t.records().map(|r| r.ev).collect();
            assert_eq!(evs.len(), 3, "{name}");
            assert!(
                matches!(evs[0], TraceEvent::PollRun { count: 2, .. }),
                "{name}: {:?}",
                evs[0]
            );
            assert!(
                matches!(evs[2], TraceEvent::MmioRead { .. }),
                "{name}: the read after the event is a new run"
            );
        }
    }

    #[test]
    fn a_sink_that_is_off_records_nothing_and_still_answers() {
        let mut t = TraceSink::default();
        assert!(!t.is_enabled());
        assert!(t.kinds().is_empty());
        drive(&mut t, true);
        assert_eq!(t.head(), 0);
        assert_eq!(t.tail(), 0);
        assert_eq!(t.bytes(), 0);
        assert_eq!(t.records().count(), 0);
        assert_eq!(t.pending(), None);
        assert_eq!(t.digest(), *blake3::Hasher::new().finalize().as_bytes());
    }

    #[test]
    fn a_kind_filter_drops_records_and_keeps_the_order_of_the_rest() {
        let mut t = TraceSink::new(TraceKinds::MMIO_WRITE.union(TraceKinds::RESET), 64);
        drive(&mut t, false);
        let evs: Vec<_> = t.records().map(|r| r.ev).collect();
        assert_eq!(
            evs,
            vec![
                TraceEvent::MmioWrite {
                    pc: LOOP_PC,
                    addr: SPI2_ST,
                    val: 0x8000_0000,
                    size: 4
                },
                TraceEvent::Reset(ResetKind::of(ResetCause::USB_UART_CHIP).expect("documented")),
            ]
        );
        assert!(t.kinds().has(TraceKinds::MMIO_WRITE));
        assert!(!t.kinds().has(TraceKinds::MMIO_READ));
    }

    #[test]
    fn the_replay_window_keeps_the_newest_records_and_the_cursors_stay_absolute() {
        let mut t = TraceSink::new(TraceKinds::ALL, 4);
        for i in 0..10u64 {
            t.mmio_write(i, LOOP_PC, SPI2_ST, i as u32, 4);
        }
        assert_eq!(t.head(), 10);
        assert_eq!(t.tail(), 6);
        let vals: Vec<_> = t
            .records()
            .map(|r| match r.ev {
                TraceEvent::MmioWrite { val, .. } => val,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(vals, vec![6, 7, 8, 9]);
        // The digest covers every record, including the evicted ones.
        let mut all = TraceSink::new(TraceKinds::ALL, 64);
        for i in 0..10u64 {
            all.mmio_write(i, LOOP_PC, SPI2_ST, i as u32, 4);
        }
        assert_eq!(t.digest(), all.digest());
    }

    #[test]
    fn a_long_busy_wait_costs_one_short_record() {
        // The official image read SPI2 4,416,291 times under QEMU: one 17-byte record.
        let mut t = TraceSink::new(TraceKinds::ALL, 4);
        t.mmio_read(0, LOOP_PC, SPI2_ST, 0, 4);
        t.poll_repeat(4_416_290);
        assert_eq!(
            t.pending(),
            Some(TraceRecord {
                insns: 0,
                ev: TraceEvent::PollRun {
                    pc: LOOP_PC,
                    addr: SPI2_ST,
                    val: 0,
                    size: 4,
                    count: 4_416_291,
                },
            })
        );
        t.flush();
        assert_eq!(t.head(), 1);
        assert_eq!(t.bytes(), 17);
        let before = t.digest();
        t.poll_repeat(10);
        assert_eq!(t.digest(), before);
        assert_eq!(t.head(), 1);
    }

    #[test]
    fn observing_the_trace_mid_poll_changes_nothing() {
        let busy_wait = |t: &mut TraceSink, observe_after: Option<usize>| {
            for i in 0..20u64 {
                t.mmio_read(100 + i, LOOP_PC, SPI2_ST, 0, 4);
                if observe_after == Some(i as usize) {
                    // What a stop, a snapshot or a slice boundary does: read the trace.
                    t.digest();
                    t.bytes();
                    t.head();
                    t.records().count();
                    t.pending();
                }
            }
            t.mmio_read(200, LOOP_PC, SPI2_ST, 1, 4);
            t.flush();
        };
        let (mut quiet, mut watched) = (sink(), sink());
        busy_wait(&mut quiet, None);
        busy_wait(&mut watched, Some(9));
        assert_eq!(quiet.digest(), watched.digest(), "the digest split the run");
        assert_eq!(quiet.head(), watched.head());
        assert_eq!(quiet.bytes(), watched.bytes());
        assert_eq!(
            quiet.records().collect::<Vec<_>>(),
            watched.records().collect::<Vec<_>>()
        );
        assert!(matches!(
            quiet.records().next().expect("a record").ev,
            TraceEvent::PollRun { count: 20, .. }
        ));

        // An open run digests and measures as it will once closed.
        let mut t = TraceSink::new(TraceKinds::ALL, 64);
        t.mmio_read(0, LOOP_PC, SPI2_ST, 0, 4);
        t.poll_repeat(999);
        let (open_digest, open_bytes) = (t.digest(), t.bytes());
        assert_eq!(t.pending().map(|r| r.insns), Some(0));
        t.flush();
        assert_eq!(t.digest(), open_digest);
        assert_eq!(t.bytes(), open_bytes);
        assert_eq!(t.head(), 1, "the run closed exactly once");
    }

    #[test]
    fn a_filtered_event_still_closes_the_open_run() {
        type Closer = fn(&mut TraceSink);
        let closers: [(&str, Closer); 3] = [
            ("write", |t| t.mmio_write(2, LOOP_PC, SPI2_ST, 1, 4)),
            ("irq", |t| {
                t.irq(
                    2,
                    IrqEvent::Take {
                        line: 7,
                        pc: 0x4038_0000,
                    },
                )
            }),
            ("reset", |t| {
                t.reset(2, ResetKind::of(ResetCause::POWERON).expect("documented"))
            }),
        ];
        for (name, close) in closers {
            // The reads-only filter of `debug trace` and the hang detector.
            let mut t = TraceSink::new(TraceKinds::MMIO_READ, 64);
            t.mmio_read(1, LOOP_PC, SPI2_ST, 0, 4);
            close(&mut t);
            t.mmio_read(3, LOOP_PC, SPI2_ST, 0, 4);
            t.flush();
            let evs: Vec<_> = t.records().map(|r| r.ev).collect();
            assert_eq!(
                evs,
                vec![
                    TraceEvent::MmioRead {
                        pc: LOOP_PC,
                        addr: SPI2_ST,
                        val: 0,
                        size: 4
                    },
                    TraceEvent::MmioRead {
                        pc: LOOP_PC,
                        addr: SPI2_ST,
                        val: 0,
                        size: 4
                    },
                ],
                "{name}: the filtered event was folded away"
            );
            assert!(
                !evs.iter().any(|e| matches!(e, TraceEvent::PollRun { .. })),
                "{name}: a busy-wait that never happened"
            );
        }
    }

    #[test]
    fn a_loop_with_two_tracked_reads_does_not_fold() {
        let mut t = TraceSink::new(TraceKinds::ALL, 64);
        for i in 0..8u64 {
            t.mmio_read(i * 2, LOOP_PC, SPI2_ST, 0, 4);
            t.mmio_read(i * 2 + 1, LOOP_PC + 4, TICKS, 0, 4);
        }
        t.flush();
        assert_eq!(t.head(), 16, "every read is its own record");
        assert!(
            t.records()
                .all(|r| matches!(r.ev, TraceEvent::MmioRead { .. })),
            "nothing folds when the reads alternate"
        );
        assert!(t.pending().is_none());
    }

    #[test]
    fn the_record_window_is_allocated_once_at_the_size_requested() {
        let want = DEFAULT_RECENT_RECORDS * 4;
        let mut t = TraceSink::new(TraceKinds::ALL, want);
        let cap = |t: &TraceSink| t.state.as_deref().expect("on").recent.capacity();
        assert!(cap(&t) >= want, "{} < {want}", cap(&t));
        let at_new = cap(&t);
        for i in 0..(want as u64 * 2) {
            t.mmio_write(i, LOOP_PC, SPI2_ST, i as u32, 4);
        }
        assert_eq!(cap(&t), at_new, "the ring reallocated during the run");
        assert_eq!(t.head(), want as u64 * 2);
        assert_eq!(t.tail(), want as u64);
        assert_eq!(t.records().count(), want);
    }

    #[test]
    fn instruction_counts_are_stored_as_non_decreasing_deltas() {
        let mut t = TraceSink::new(TraceKinds::ALL, 64);
        t.mmio_write(100, LOOP_PC, SPI2_ST, 0, 4);
        t.mmio_write(50, LOOP_PC, SPI2_ST, 1, 4); // out of order: clamped, never negative
        t.mmio_write(300, LOOP_PC, SPI2_ST, 2, 4);
        let insns: Vec<_> = t.records().map(|r| r.insns).collect();
        assert_eq!(insns, vec![100, 100, 300]);
        assert!(insns.windows(2).all(|w| w[0] <= w[1]));
    }
}
