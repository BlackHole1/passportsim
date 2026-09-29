//! Our side of a boot-phase oracle comparison: a [`pemu_verify::phase::PhaseRecord`] from a real
//! [`Machine`] run, in the shape the QEMU oracle's log is filtered into.
//!
//! Function entries come from breakpoints on the watched addresses; accesses from the MMIO trace,
//! drained after every stop and slice so each entry lands between its accesses in run order. A
//! folded poll run is expanded to its reads, as the oracle executes every iteration. Neither
//! observation changes guest state.

use pemu_core::trace::{TraceEvent, TraceKinds};
use pemu_machine::config::TraceCfg;
use pemu_machine::machine::Machine;
use pemu_machine::run::RunLimits;
use pemu_machine::stops::{StopReason, StopSet};
use pemu_verify::phase::{Access, Event, PhaseRecord, Watch};
use pemu_verify::qemu_ingest::Kind;

/// The trace configuration a phase record needs: MMIO reads and writes, with a window that
/// holds more than one slice of them.
pub fn phase_trace() -> TraceCfg {
    TraceCfg {
        kinds: Some(TraceKinds::MMIO_WRITE.union(TraceKinds::MMIO_READ)),
        recent: 1 << 16,
    }
}

/// Instructions one run call may retire between two drains of the trace window. The window of
/// [`phase_trace`] holds 65,536 records and no instruction makes more than one access, so a
/// slice this long can never overrun it (and [`record_phase`] asserts that it did not).
const SLICE: u64 = 20_000;

/// Runs `m` until the first entry into `end` (an address of `watch`), recording every entry into
/// `watch` and every MMIO access. `m` must have been built with [`phase_trace`]. `Err` says why
/// the phase did not end (budget exhausted, or another stop) and the PC it stopped at.
pub fn record_phase(
    m: &mut Machine,
    watch: &Watch,
    end: u32,
    max_insns: u64,
) -> Result<PhaseRecord, String> {
    if !watch.contains_key(&end) {
        return Err(format!(
            "the end address {end:#010x} is not in the watched set"
        ));
    }
    let mut record = PhaseRecord::new("pemu");
    let stops = StopSet {
        breakpoints: watch.keys().copied().collect(),
        ..StopSet::default()
    };
    let mut cursor = m.trace().head();
    let mut left = max_insns;
    loop {
        let out = m.run(RunLimits {
            until: None,
            max_insns: Some(left.min(SLICE)),
            stops: stops.clone(),
        });
        let (tail, head) = (m.trace().tail(), m.trace().head());
        if tail > cursor {
            return Err(format!(
                "the MMIO trace window dropped {} records between two drains",
                tail - cursor
            ));
        }
        let skip = usize::try_from(cursor - tail).expect("the window fits memory");
        for rec in m.trace().records().skip(skip) {
            let (kind, pc, addr, val, size, count) = match rec.ev {
                TraceEvent::MmioWrite {
                    pc,
                    addr,
                    val,
                    size,
                } => (Kind::Write, pc, addr, val, size, 1),
                TraceEvent::MmioRead {
                    pc,
                    addr,
                    val,
                    size,
                } => (Kind::Read, pc, addr, val, size, 1),
                TraceEvent::PollRun {
                    pc,
                    addr,
                    val,
                    size,
                    count,
                } => (Kind::Read, pc, addr, val, size, count),
                TraceEvent::Irq(_) | TraceEvent::Reset(_) => continue,
            };
            for _ in 0..count {
                record.events.push(Event::Access(Access {
                    kind,
                    addr,
                    value: u64::from(val),
                    size,
                    pc: Some(pc),
                }));
            }
        }
        cursor = head;
        left = left.saturating_sub(out.insns);
        match out.reason {
            StopReason::Breakpoint(pc) => {
                let name = watch
                    .get(&pc)
                    .expect("a breakpoint of this run is a watched address");
                record.events.push(Event::Call(name.clone()));
                if pc == end {
                    return Ok(record);
                }
            }
            StopReason::MaxInsns if left > 0 => {}
            StopReason::MaxInsns => {
                return Err(format!(
                    "{max_insns} instructions ran without reaching {end:#010x} (pc {:#010x})",
                    m.hart().pc
                ));
            }
            other => {
                return Err(format!(
                    "the run stopped with {other:?} at pc {:#010x} before reaching {end:#010x}",
                    m.hart().pc
                ));
            }
        }
    }
}
