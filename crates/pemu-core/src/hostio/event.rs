//! The event ring of `HostIo::events`: fixed-size records a host reads by absolute cursor.

use serde::{Deserialize, Serialize};

use super::ring::{Ring, RingRead, RingRestoreError, RingSlices};
use crate::time::VTime;

#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum EventKind {
    #[default]
    Reset,
    Panic,
    Sleep,
    Power,
    Frame,
    UiSettled,
    FidelityWarning,
}

/// One record of `HostIo::events`, fixed-size so the ring maps onto a typed-array view. `arg` is a
/// kind-specific word (a reset cause, a frame generation); full payloads such as a panic capture
/// travel in the stop reason.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct HostEvent {
    pub kind: EventKind,
    /// Virtual time.
    pub vt: VTime,
    pub arg: u64,
}

/// Output ring of [`HostEvent`]s numbered with absolute event cursors; emitting evicts the oldest
/// event when full.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct EventRing {
    events: Ring<HostEvent>,
}

impl EventRing {
    /// An empty ring keeping at most `capacity` events.
    pub fn new(capacity: usize) -> Self {
        EventRing {
            events: Ring::new(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.events.capacity()
    }

    /// Total events ever emitted.
    pub fn head(&self) -> u64 {
        self.events.head
    }

    pub fn tail(&self) -> u64 {
        self.events.tail
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Address of the event buffer; it never changes for the life of the ring.
    pub fn as_ptr(&self) -> *const HostEvent {
        self.events.as_ptr()
    }

    /// Evicts the oldest event when full. Allocates nothing.
    pub fn emit(&mut self, event: HostEvent) {
        self.events.write(&[event]);
    }

    /// Copies events from `cursor` (cursor rules of [`ByteRing::read`](super::ByteRing::read)).
    pub fn read(&self, cursor: u64, out: &mut [HostEvent]) -> RingRead {
        self.events.read(cursor, out)
    }

    pub fn slices(&self, cursor: u64) -> RingSlices<'_, HostEvent> {
        self.events.slices(cursor)
    }

    /// Replaces the window with `events` from absolute event cursor `tail`, in place. Allocates
    /// nothing; on error nothing changes.
    pub fn restore(&mut self, tail: u64, events: &[HostEvent]) -> Result<(), RingRestoreError> {
        self.events.restore(tail, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostio::HostIo;

    #[test]
    fn event_ring_numbers_events_and_restores_in_place() {
        let mut io = HostIo::new(128);
        assert_eq!(io.events.capacity(), 2);
        let ev = |kind, ms| HostEvent {
            kind,
            vt: VTime::from_ms(ms),
            arg: ms,
        };
        io.events.emit(ev(EventKind::Reset, 1));
        io.events.emit(ev(EventKind::Frame, 2));
        io.events.emit(ev(EventKind::UiSettled, 3));
        assert_eq!(
            (io.events.tail(), io.events.head(), io.events.len()),
            (1, 3, 2)
        );
        let mut out = [HostEvent::default(); 4];
        let r = io.events.read(0, &mut out);
        assert_eq!((r.n, r.next, r.dropped), (2, 3, 1));
        assert_eq!(
            (out[0], out[1]),
            (ev(EventKind::Frame, 2), ev(EventKind::UiSettled, 3))
        );

        let saved: Vec<HostEvent> = io.events.slices(0).iter().copied().collect();
        let mut fresh = EventRing::new(2);
        let ptr = fresh.as_ptr();
        fresh.restore(io.events.tail(), &saved).unwrap();
        assert_eq!(fresh, io.events);
        assert_eq!(fresh.as_ptr(), ptr);
        assert!(fresh.restore(0, &[HostEvent::default(); 3]).is_err());
        assert_eq!(fresh, io.events);
    }
}
