//! Stop conditions of `Machine::run`, each checked only when what it watches can have changed and
//! each ending the run at the exact instruction boundary where it fired, whatever the block size or
//! slice (`specs/notes/g3-behavior.md`):
//!
//! | Stop | Mechanism | Ends the run |
//! |---|---|---|
//! | breakpoint | a `K_HOOK` terminator at the pc | before the instruction executes; the next run executes it once |
//! | write watch | `PF_SLOW` on every view of the watched pages, matched by arena offset | right after the writing instruction |
//! | serial matcher | a newline appended to the stream | after the instruction that completed the line |
//! | event matcher | a record appended to `HostIo::events` | at the boundary the event was emitted |
//!
//! A reset stop is an event matcher on `EventKind::Reset`. Matching only turns an `Ok` access into
//! `OkStop`, so it changes no guest-visible state.
//!
//! A breakpoint stops before any hook bound at its pc runs, and the resume dispatches that hook
//! once. On a tripwire pc the stops alternate `Breakpoint` then `Tripwire` without progress,
//! because a tripwire never lets the instruction run.

use pemu_core::fidelity::FirstTouch;
use pemu_core::hostio::{EventKind, HostEvent, HostIo, SerialStream};
use pemu_core::reset::ResetKind;
use pemu_core::time::VTime;
use pemu_hle::guest_call::HleError;
use pemu_rv32::engine::HaltCause;
use pemu_soc_c3::periph::rtc_sleep::SleepKind;

use crate::hang::StuckReport;

/// Why `Machine::run` returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    Until,
    MaxInsns,
    Matcher(MatcherId),
    Breakpoint(u32),
    Watchpoint {
        addr: u32,
        pc: u32,
    },
    GuestPanic(PanicCapture),
    /// The WFI hart can never be woken by the guest itself: wait for input. Reported where the wait
    /// became unwakeable, also under an `until` limit and with events pending, so running on to a
    /// later limit returns it again at the same instant.
    Deadlock,
    Stuck(StuckReport),
    Tripwire(TripReport),
    Unmodeled(FirstTouch),
    Hle(HleError),
    Halted(HaltCause),
    /// Reserved; never produced: every reset is sequenced (`apply.rs`) and a caller stops at one
    /// with `Matcher::Event(EventKind::Reset)`. Kept so the wasm stop codes do not move.
    ChipReset(ResetKind),
    /// Reserved; never produced: the machine performs `Wiring::SleepEnter` itself (`sleep.rs`), and
    /// a restored snapshot carrying it gets the owed sleep performed.
    Sleep(SleepKind),
}

/// Stops armed for one run: breakpoints, write watches and matchers. `pemu-api`'s matcher
/// compiler targets this shape.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StopSet {
    pub breakpoints: Vec<u32>,
    pub watches: Vec<Watch>,
    pub matchers: Vec<(MatcherId, Matcher)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopSetError {
    UnwatchableRange { addr: u32, len: u32 },
}

impl std::fmt::Display for StopSetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StopSetError::UnwatchableRange { addr, len } => write!(
                f,
                "cannot watch {len} bytes at {addr:#010x}: only SRAM and RTC fast RAM are watchable"
            ),
        }
    }
}

impl std::error::Error for StopSetError {}

impl StopSet {
    /// Refuses a watch outside writable arena RAM or one running past its region's end.
    /// `Machine::run` never arms a watch this refuses, so an unchecked one never fires.
    pub fn check(&self) -> Result<(), StopSetError> {
        for w in &self.watches {
            if !w.watchable() {
                return Err(StopSetError::UnwatchableRange {
                    addr: w.addr,
                    len: w.len,
                });
            }
        }
        Ok(())
    }
}

impl Watch {
    pub fn watchable(&self) -> bool {
        let len = self.len.max(1);
        let Some(last) = self.addr.checked_add(len - 1) else {
            return false;
        };
        match pemu_soc_c3::mem::region_of(self.addr) {
            Some(region) => region.flags & pemu_rv32::bus::PF_W != 0 && region.covers(last),
            None => false,
        }
    }
}

/// A write watch: `len` bytes from `addr` (0 is read as 1).
///
/// Writes through any view of the same memory match (a DRAM watch sees an IRAM-alias write).
/// Only CPU writes to arena RAM (SRAM0, SRAM1, RTC fast RAM) are watched: a ROM store changes
/// nothing, and flash, register and DMA writes do not pass the store path a watch uses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Watch {
    pub addr: u32,
    pub len: u32,
}

/// What a matcher waits for: the leaf forms of `pemu-api::matchers` the machine can decide from
/// its own state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Matcher {
    Serial {
        stream: SerialStream,
        pattern: LinePattern,
    },
    Event(EventKind),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinePattern {
    Contains(String),
    Prefix(String),
    Exact(String),
}

impl LinePattern {
    pub fn matches(&self, line: &[u8]) -> bool {
        match self {
            LinePattern::Contains(t) => {
                let t = t.as_bytes();
                t.is_empty() || line.windows(t.len()).any(|w| w == t)
            }
            LinePattern::Prefix(t) => line.starts_with(t.as_bytes()),
            LinePattern::Exact(t) => line == t.as_bytes(),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MatcherId(pub u32);

/// Captured guest panic, raw: decoding belongs to `pemu-introspect`. All zero if no hook produced
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PanicCapture {
    pub pc: u32,
    /// `a0` to `a3` at the hook (`esp_panic_handler(panic_info_t *info)` takes `a0`).
    pub args: [u32; 4],
    /// The `abort` or `__assert_func` entry the guest passed through first, if any, with `a0` to
    /// `a3` at that entry.
    pub first: Option<(pemu_hle::observe::ObserveKind, [u32; 4])>,
}

/// Which ESP-IDF watchdog a fault came from. The task watchdog is TIMG0's MWDT and the interrupt
/// watchdog TIMG1's, so the group whose stage interrupt fired is the answer, no console text
/// needed.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Watchdog {
    Task,
    Interrupt,
}

impl Watchdog {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Watchdog::Task => "task",
            Watchdog::Interrupt => "interrupt",
        }
    }

    #[must_use]
    pub fn timer_group(self) -> &'static str {
        match self {
            Watchdog::Task => "timg0",
            Watchdog::Interrupt => "timg1",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct WatchdogFire {
    pub which: Watchdog,
    pub at: VTime,
    /// The guest has not fed that watchdog since. The ESP-IDF panic handler feeds both before it
    /// reports anything (`esp_panic_handler_reconfigure_wdts`), so a decoded fault usually has a
    /// fed watchdog behind it; `at` is how a caller judges whether it is this fault's.
    pub unfed: bool,
}

/// Report of a tripped tripwire: the kind, the symbol and caller, and the feature a
/// `DisabledFeature` tripwire stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TripReport {
    pub kind: pemu_hle::tripwire::TripKind,
    pub pc: u32,
    pub detail: String,
    pub caller: u32,
    pub feature: Option<&'static str>,
}

impl Default for TripReport {
    fn default() -> TripReport {
        TripReport {
            kind: pemu_hle::tripwire::TripKind::BlobInternal,
            pc: 0,
            detail: String::new(),
            caller: 0,
            feature: None,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ArmedStops {
    /// Watched ranges as arena offsets, `[start, end)`, so every view of a byte matches.
    pub(crate) watches: Vec<(usize, usize)>,
    pub(crate) watch_hit: Option<(u32, u32)>,
    pub(crate) slow_pages: Vec<u32>,
    pub(crate) serial_armed: [bool; SerialStream::ALL.len()],
    pub(crate) line_start: [u64; SerialStream::ALL.len()],
    pub(crate) event_cursor: u64,
    pub(crate) any_matcher: bool,
}

impl ArmedStops {
    pub(crate) fn watched(&self, addr: u32, size: u8) -> bool {
        if self.watches.is_empty() {
            return false;
        }
        let Some(at) = pemu_soc_c3::mem::arena_offset(addr) else {
            return false;
        };
        let end = at + usize::from(size);
        self.watches.iter().any(|&(s, e)| at < e && s < end)
    }

    pub(crate) fn wants_uart0_lines(&self) -> bool {
        self.serial_armed[SerialStream::Uart0Tx.index()]
    }

    /// Arms `set` against the output so far: serial cursors at the start of the line in progress
    /// and the event cursor at the head, so a matcher sees only lines and events completed during
    /// the run.
    pub(crate) fn arm(&mut self, set: &StopSet, io: &HostIo) {
        self.serial_armed = [false; SerialStream::ALL.len()];
        self.any_matcher = !set.matchers.is_empty();
        for (_, m) in &set.matchers {
            if let Matcher::Serial { stream, .. } = m {
                self.serial_armed[stream.index()] = true;
            }
        }
        for stream in SerialStream::ALL {
            self.line_start[stream.index()] = line_begin(io, stream);
        }
        self.event_cursor = io.events.head();
        self.watch_hit = None;
    }

    /// The first matcher satisfied by output appended since the last call, serial lines before
    /// events. Every new line and event is consumed, matched or not, so none is tested twice.
    pub(crate) fn check_matchers(
        &mut self,
        io: &HostIo,
        matchers: &[(MatcherId, Matcher)],
    ) -> Option<MatcherId> {
        if !self.any_matcher {
            return None;
        }
        let mut found = None;
        for stream in SerialStream::ALL {
            let i = stream.index();
            if !self.serial_armed[i] {
                continue;
            }
            let ring = io.serial_ring(stream);
            let from = self.line_start[i].max(ring.tail());
            if from >= ring.head() {
                continue;
            }
            let bytes: Vec<u8> = ring.slices(from).iter().copied().collect();
            let mut start = 0usize;
            for pos in bytes
                .iter()
                .enumerate()
                .filter_map(|(pos, b)| (*b == b'\n').then_some(pos))
            {
                let mut line = &bytes[start..pos];
                if let [rest @ .., b'\r'] = line {
                    line = rest;
                }
                start = pos + 1;
                if found.is_none() {
                    found = matchers.iter().find_map(|(id, m)| match m {
                        Matcher::Serial { stream: s, pattern } if *s == stream => {
                            pattern.matches(line).then_some(*id)
                        }
                        _ => None,
                    });
                }
            }
            self.line_start[i] = from + start as u64;
        }
        let head = io.events.head();
        while self.event_cursor < head {
            let cursor = self.event_cursor.max(io.events.tail());
            let mut one = [HostEvent::default()];
            io.events.read(cursor, &mut one);
            self.event_cursor = cursor + 1;
            if found.is_none() {
                found = matchers.iter().find_map(|(id, m)| match m {
                    Matcher::Event(kind) if *kind == one[0].kind => Some(*id),
                    _ => None,
                });
            }
        }
        found
    }
}

fn line_begin(io: &HostIo, stream: SerialStream) -> u64 {
    let ring = io.serial_ring(stream);
    let marks = io.lines.slices(stream, io.lines.tail(stream));
    marks
        .iter()
        .last()
        .map_or(ring.tail(), |m| (m.offset + 1).max(ring.tail()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::time::VTime;

    #[test]
    fn a_watch_outside_writable_ram_is_refused() {
        use pemu_soc_c3::mem::{RTC_FAST_BASE, RTC_FAST_LEN, SRAM1_DRAM_BASE};
        let set = |addr, len| StopSet {
            watches: vec![Watch { addr, len }],
            ..StopSet::default()
        };
        for (addr, len) in [(SRAM1_DRAM_BASE, 4), (0x4038_2000, 1), (RTC_FAST_BASE, 0)] {
            assert_eq!(set(addr, len).check(), Ok(()), "{addr:#x}");
        }
        for (addr, len) in [
            (0x4000_0000, 4),                      // ROM
            (0x3FF0_0000, 4),                      // ROM data view
            (0x4200_0000, 4),                      // flash, instruction window
            (0x3C00_0000, 4),                      // flash, data window
            (0x6000_0000, 4),                      // UART0
            (RTC_FAST_BASE + RTC_FAST_LEN - 2, 4), // runs off the end of RTC fast RAM
            (u32::MAX, 2),                         // wraps
        ] {
            assert_eq!(
                set(addr, len).check(),
                Err(StopSetError::UnwatchableRange { addr, len }),
                "{addr:#x}"
            );
        }
    }

    #[test]
    fn line_patterns_compare_the_line_without_its_ending() {
        let line = b"entry 0x403cbf1a";
        assert!(LinePattern::Prefix("entry 0x".into()).matches(line));
        assert!(!LinePattern::Prefix("ntry".into()).matches(line));
        assert!(LinePattern::Contains("cbf1".into()).matches(line));
        assert!(LinePattern::Exact("entry 0x403cbf1a".into()).matches(line));
        assert!(!LinePattern::Exact("entry".into()).matches(line));
    }

    #[test]
    fn a_serial_matcher_fires_on_a_line_completed_after_arming() {
        let mut io = HostIo::new(1024);
        io.serial_write(SerialStream::UsjTx, b"entry 0x1\r\npart", VTime(1));
        let set = StopSet {
            matchers: vec![(
                MatcherId(7),
                Matcher::Serial {
                    stream: SerialStream::UsjTx,
                    pattern: LinePattern::Prefix("partial entry".into()),
                },
            )],
            ..StopSet::default()
        };
        let mut armed = ArmedStops::default();
        armed.arm(&set, &io);
        assert_eq!(armed.check_matchers(&io, &set.matchers), None);
        io.serial_write(SerialStream::UsjTx, b"ial entry 2\r\n", VTime(2));
        assert_eq!(
            armed.check_matchers(&io, &set.matchers),
            Some(MatcherId(7)),
            "the line in progress at arming counts from its first byte"
        );
        assert_eq!(armed.check_matchers(&io, &set.matchers), None);
    }

    #[test]
    fn an_event_matcher_fires_on_its_kind_only() {
        let mut io = HostIo::new(64);
        let set = StopSet {
            matchers: vec![(MatcherId(1), Matcher::Event(EventKind::Reset))],
            ..StopSet::default()
        };
        let mut armed = ArmedStops::default();
        armed.arm(&set, &io);
        io.events.emit(HostEvent {
            kind: EventKind::Power,
            vt: VTime(1),
            arg: 1,
        });
        assert_eq!(armed.check_matchers(&io, &set.matchers), None);
        io.events.emit(HostEvent {
            kind: EventKind::Reset,
            vt: VTime(2),
            arg: 0x15,
        });
        assert_eq!(armed.check_matchers(&io, &set.matchers), Some(MatcherId(1)));
    }
}
