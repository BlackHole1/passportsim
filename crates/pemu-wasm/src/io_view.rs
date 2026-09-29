//! The published view of [`HostIo`] that `pemu_io_layout` hands the worker: one [`IoLayout`], one
//! cursor block and the `repr(C)` mirrors of the record rings.
//!
//! Every buffer is allocated once and never reallocated, so the worker's typed arrays survive
//! every run; only memory growth or [`IoPublisher::rebuilt`] invalidates them, and the latter
//! bumps the generation. [`IoPublisher::refresh`] runs once as an ABI call returns, on the
//! worker's own thread, so nothing observes a half-written block.

use pemu_core::hostio::{HostIo, PcmRing, SerialStream};
use pemu_core::time::VTime;

use crate::layout::{
    ABI_VERSION, CURSOR_SLOTS, DEFAULT_BACKLIGHT_SCALE, DIRTY_NONE, FRAME_FLAG_DISPLAY_ON,
    FRAME_FLAG_GLASS_COMPLEMENT, FRAME_FLAG_INVERTED, FRAME_FLAG_POWERED, FRAME_FLAG_SLEEPING,
    HostEventAbi, IoLayout, LineMarkAbi, PcmRecordAbi, RingId, RingLayout,
    SLOT_AUDIO_IN_UNDERFLOWS, SLOT_AUDIO_OUT_UNDERFLOWS, SLOT_FRAME_GENERATION, SLOT_NOW_PS,
    event_kind_code, serial_stream_code,
};

/// The `repr(C)` copy of one ring of `pemu-core` records, addressed as the core addresses it
/// (slot `pos % capacity`), plus the head it was last filled to.
struct Mirror<T> {
    slots: Box<[T]>,
    published_head: u64,
}

impl<T: Copy + Default> Mirror<T> {
    fn new(capacity: usize) -> Self {
        Mirror {
            slots: vec![T::default(); capacity.max(1)].into_boxed_slice(),
            published_head: 0,
        }
    }

    fn capacity(&self) -> usize {
        self.slots.len()
    }

    fn ptr(&self) -> *const T {
        self.slots.as_ptr()
    }

    /// The positions still to convert for `[tail, head)`: everything since `published_head`, or
    /// the whole window when the tail moved backwards (a restore) or the mirror was invalidated.
    fn pending(&mut self, tail: u64, head: u64) -> core::ops::Range<u64> {
        let from = if self.published_head < tail || self.published_head > head {
            tail
        } else {
            self.published_head.max(tail)
        };
        self.published_head = head;
        from..head
    }

    fn invalidate(&mut self) {
        self.published_head = 0;
    }

    fn put(&mut self, pos: u64, value: T) {
        let cap = self.capacity() as u64;
        let slot = (pos % cap) as usize;
        self.slots[slot] = value;
    }
}

/// Everything `pemu_io_layout` points at, owned by one machine instance.
pub struct IoPublisher {
    layout: Box<IoLayout>,
    cursors: Box<[u64]>,
    audio_out_records: Mirror<PcmRecordAbi>,
    audio_in_records: Mirror<PcmRecordAbi>,
    events: Mirror<HostEventAbi>,
    lines: [Mirror<LineMarkAbi>; SerialStream::ALL.len()],
}

impl IoPublisher {
    /// Allocates the view for `io` and fills every address and capacity. The generation starts at
    /// 1, so a worker that recorded 0 always re-reads.
    pub fn new(io: &HostIo) -> Self {
        let mut publisher = IoPublisher {
            layout: Box::new(IoLayout::default()),
            cursors: vec![0u64; CURSOR_SLOTS as usize].into_boxed_slice(),
            audio_out_records: Mirror::new(io.audio_out.record_capacity()),
            audio_in_records: Mirror::new(io.audio_in.record_capacity()),
            events: Mirror::new(io.events.capacity()),
            lines: [
                Mirror::new(io.lines.capacity(SerialStream::UsjTx)),
                Mirror::new(io.lines.capacity(SerialStream::Uart0Tx)),
            ],
        };
        publisher.layout.generation = 1;
        publisher.wire(io);
        publisher
    }

    /// Re-reads every buffer address after something that may have moved one and bumps the
    /// generation, so the worker re-creates its views before the next read.
    pub fn rebuilt(&mut self, io: &HostIo) {
        self.audio_out_records.invalidate();
        self.audio_in_records.invalidate();
        self.events.invalidate();
        for mirror in &mut self.lines {
            mirror.invalidate();
        }
        self.layout.generation = self.layout.generation.wrapping_add(1).max(1);
        self.wire(io);
    }

    /// The pointer `pemu_io_layout` returns, stable for the life of the publisher.
    pub fn layout_ptr(&self) -> *const IoLayout {
        &*self.layout
    }

    pub fn layout(&self) -> &IoLayout {
        &self.layout
    }

    pub fn cursors(&self) -> &[u64] {
        &self.cursors
    }

    fn set_ring(&mut self, ring: RingId, buf_ptr: *const u8, capacity: usize, elem_size: usize) {
        self.layout.rings[ring as usize] = RingLayout {
            buf_ptr: buf_ptr as usize as u32,
            capacity: capacity as u32,
            elem_size: elem_size as u32,
            cursor_slot: ring.head_slot(),
        };
    }

    fn wire(&mut self, io: &HostIo) {
        self.layout.abi_version = ABI_VERSION;
        self.layout.cursors_ptr = self.cursors.as_ptr() as usize as u32;
        self.layout.cursor_slots = CURSOR_SLOTS;
        self.layout.frame_ptr = io.frame.as_ptr() as usize as u32;
        self.layout.frame_width = io.frame.width() as u32;
        self.layout.frame_height = io.frame.height() as u32;

        let byte_rings = [
            (RingId::UsjTx, &io.usj_tx),
            (RingId::UsjRx, &io.usj_rx),
            (RingId::Uart0Tx, &io.uart0_tx),
        ];
        for (ring, bytes) in byte_rings {
            self.set_ring(ring, bytes.as_ptr(), bytes.capacity(), 1);
        }
        let samples = [
            (RingId::AudioOutSamples, &io.audio_out),
            (RingId::AudioInSamples, &io.audio_in),
        ];
        for (ring, pcm) in samples {
            self.set_ring(ring, pcm.as_ptr().cast(), pcm.capacity(), 2);
        }
        let record_mirrors = [
            (RingId::AudioOutRecords, 0usize),
            (RingId::AudioInRecords, 1usize),
        ];
        for (ring, which) in record_mirrors {
            let mirror = if which == 0 {
                &self.audio_out_records
            } else {
                &self.audio_in_records
            };
            let (ptr, cap) = (mirror.ptr(), mirror.capacity());
            self.set_ring(ring, ptr.cast(), cap, size_of::<PcmRecordAbi>());
        }
        let (ptr, cap) = (self.events.ptr(), self.events.capacity());
        self.set_ring(RingId::Events, ptr.cast(), cap, size_of::<HostEventAbi>());
        for (index, ring) in [RingId::LinesUsjTx, RingId::LinesUart0Tx]
            .into_iter()
            .enumerate()
        {
            let mirror = &self.lines[index];
            let (ptr, cap) = (mirror.ptr(), mirror.capacity());
            self.set_ring(ring, ptr.cast(), cap, size_of::<LineMarkAbi>());
        }
    }

    /// Copies the cursors, the frame state and every record the worker has not seen yet, just
    /// before an ABI call that can have changed `io` returns.
    ///
    /// **The dirty span is taken here, and only here**, so a published span is the rows changed
    /// since the previous refresh. `Machine::run` must not call `take_dirty` (the panel would never
    /// update), and a host that throttles uploads unions the spans it skipped.
    pub fn refresh(&mut self, io: &mut HostIo, now: VTime) {
        let byte_rings = [
            (RingId::UsjTx, &io.usj_tx),
            (RingId::UsjRx, &io.usj_rx),
            (RingId::Uart0Tx, &io.uart0_tx),
        ];
        for (ring, bytes) in byte_rings {
            self.cursors[ring.head_slot() as usize] = bytes.head();
            self.cursors[ring.tail_slot() as usize] = bytes.tail();
        }
        self.refresh_pcm(
            RingId::AudioOutSamples,
            RingId::AudioOutRecords,
            &io.audio_out,
        );
        self.refresh_pcm(RingId::AudioInSamples, RingId::AudioInRecords, &io.audio_in);
        self.refresh_events(io);
        self.refresh_lines(io);

        self.cursors[SLOT_FRAME_GENERATION as usize] = io.frame.generation();
        self.cursors[SLOT_AUDIO_OUT_UNDERFLOWS as usize] = io.audio_out.underflows();
        self.cursors[SLOT_AUDIO_IN_UNDERFLOWS as usize] = io.audio_in.underflows();
        self.cursors[SLOT_NOW_PS as usize] = now.0;

        // Brightness is `(duty >> 4, 1 << duty_res)` and `FramePort::backlight` is the raw LEDC
        // duty, so the shift happens here to keep numerator and denominator on one scale.
        self.layout.frame_backlight = u32::from(io.frame.backlight()) >> 4;
        self.layout.frame_backlight_scale = DEFAULT_BACKLIGHT_SCALE;
        let mut flags = 0;
        if io.frame.powered() {
            flags |= FRAME_FLAG_POWERED;
        }
        if io.frame.sleeping() {
            flags |= FRAME_FLAG_SLEEPING;
        }
        if io.frame.inverted() {
            flags |= FRAME_FLAG_INVERTED;
        }
        if io.frame.display_on() {
            flags |= FRAME_FLAG_DISPLAY_ON;
        }
        if io.frame.glass_complement() {
            flags |= FRAME_FLAG_GLASS_COMPLEMENT;
        }
        self.layout.frame_flags = flags;
        match io.frame.take_dirty() {
            Some((first, last)) => {
                self.layout.frame_dirty_first = u32::from(first);
                self.layout.frame_dirty_last = u32::from(last);
            }
            None => {
                self.layout.frame_dirty_first = DIRTY_NONE;
                self.layout.frame_dirty_last = 0;
            }
        }
    }

    fn refresh_pcm(&mut self, samples: RingId, records: RingId, pcm: &PcmRing) {
        self.cursors[samples.head_slot() as usize] = pcm.head();
        self.cursors[samples.tail_slot() as usize] = pcm.tail();
        self.cursors[records.head_slot() as usize] = pcm.record_head();
        self.cursors[records.tail_slot() as usize] = pcm.record_tail();
        let mirror = match records {
            RingId::AudioOutRecords => &mut self.audio_out_records,
            _ => &mut self.audio_in_records,
        };
        let pending = mirror.pending(pcm.record_tail(), pcm.record_head());
        if pending.is_empty() {
            return;
        }
        // By record position, as the other mirrors do. `PcmRing::record_at` takes a *sample*
        // cursor: it answers record 0 for position 1, or `None` once the sample ring evicted it,
        // and a skipped slot is lost for good, so a runtime format change never reaches the
        // playback worklet, which then underruns (about 660 underruns over a 10 s body).
        let view = pcm.record_slices(pending.start);
        let mut pos = view.start;
        for record in view.iter() {
            mirror.put(
                pos,
                PcmRecordAbi {
                    vt_start_ps: record.vt_start.0 as i64,
                    first: record.first,
                    fs: record.fs,
                    channels: u32::from(record.channels),
                },
            );
            pos += 1;
        }
    }

    fn refresh_events(&mut self, io: &HostIo) {
        self.cursors[RingId::Events.head_slot() as usize] = io.events.head();
        self.cursors[RingId::Events.tail_slot() as usize] = io.events.tail();
        let pending = self.events.pending(io.events.tail(), io.events.head());
        if pending.is_empty() {
            return;
        }
        let view = io.events.slices(pending.start);
        let mut pos = view.start;
        for event in view.iter() {
            self.events.put(
                pos,
                HostEventAbi {
                    vt_ps: event.vt.0 as i64,
                    arg: event.arg,
                    kind: event_kind_code(event.kind),
                    reserved: 0,
                },
            );
            pos += 1;
        }
    }

    fn refresh_lines(&mut self, io: &HostIo) {
        for (index, stream) in SerialStream::ALL.into_iter().enumerate() {
            let ring = match stream {
                SerialStream::UsjTx => RingId::LinesUsjTx,
                SerialStream::Uart0Tx => RingId::LinesUart0Tx,
            };
            let (head, tail) = (io.lines.head(stream), io.lines.tail(stream));
            self.cursors[ring.head_slot() as usize] = head;
            self.cursors[ring.tail_slot() as usize] = tail;
            let pending = self.lines[index].pending(tail, head);
            if pending.is_empty() {
                continue;
            }
            let view = io.lines.slices(stream, pending.start);
            let mut pos = view.start;
            for mark in view.iter() {
                self.lines[index].put(
                    pos,
                    LineMarkAbi {
                        offset: mark.offset,
                        vt_ps: mark.vt.0 as i64,
                        stream: serial_stream_code(mark.stream),
                        reserved: 0,
                    },
                );
                pos += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use pemu_core::hostio::{EventKind, HostEvent};

    use super::*;
    use crate::layout::{FRAME_HEIGHT, FRAME_WIDTH, RING_COUNT};

    fn io() -> HostIo {
        HostIo::new(4096)
    }

    #[test]
    fn every_ring_is_wired_with_a_buffer_a_capacity_and_its_own_cursor_cells() {
        let io = io();
        let publisher = IoPublisher::new(&io);
        let layout = publisher.layout();
        assert_eq!(layout.abi_version, ABI_VERSION);
        assert_eq!(layout.cursor_slots, CURSOR_SLOTS);
        assert_eq!(layout.frame_width, FRAME_WIDTH);
        assert_eq!(layout.frame_height, FRAME_HEIGHT);
        assert_ne!(layout.frame_ptr, 0);
        assert_ne!(layout.cursors_ptr, 0);
        for ring in RingId::ALL {
            let entry = layout.rings[ring as usize];
            assert_ne!(entry.buf_ptr, 0, "{ring:?} has no buffer");
            assert!(entry.capacity > 0, "{ring:?} has no capacity");
            assert!(entry.elem_size > 0, "{ring:?} has no element size");
            assert_eq!(entry.cursor_slot, ring.head_slot());
        }
        assert_eq!(layout.rings.len(), RING_COUNT);
        assert_eq!(layout.rings[RingId::UsjTx as usize].elem_size, 1);
        assert_eq!(layout.rings[RingId::AudioOutSamples as usize].elem_size, 2);
        assert_eq!(layout.rings[RingId::Events as usize].elem_size, 24);
    }

    #[test]
    fn buffer_addresses_survive_a_refresh_but_a_rebuild_bumps_the_generation() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        let before = *publisher.layout();
        io.serial_write(SerialStream::UsjTx, b"boot\n", VTime(7));
        publisher.refresh(&mut io, VTime(7));
        let after = *publisher.layout();
        assert_eq!(
            after.generation, before.generation,
            "a run never moves a buffer"
        );
        assert_eq!(after.rings, before.rings);
        publisher.rebuilt(&io);
        assert_ne!(publisher.layout().generation, before.generation);
        assert_ne!(
            publisher.layout().generation,
            0,
            "0 is the worker's never-read value"
        );
    }

    #[test]
    fn serial_bytes_and_line_marks_reach_the_cursor_block_and_the_mirror() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        io.serial_write(SerialStream::UsjTx, b"ready\n", VTime(1_000));
        io.serial_write(SerialStream::Uart0Tx, b"rst\n", VTime(2_000));
        publisher.refresh(&mut io, VTime(2_000));

        let cursors = publisher.cursors();
        assert_eq!(cursors[RingId::UsjTx.head_slot() as usize], 6);
        assert_eq!(cursors[RingId::UsjTx.tail_slot() as usize], 0);
        assert_eq!(cursors[RingId::LinesUsjTx.head_slot() as usize], 1);
        assert_eq!(cursors[RingId::LinesUart0Tx.head_slot() as usize], 1);
        assert_eq!(cursors[SLOT_NOW_PS as usize], 2_000);

        let mark = publisher.lines[0].slots[0];
        assert_eq!(mark.offset, 5, "the cursor of the newline byte");
        assert_eq!(mark.vt_ps, 1_000);
        assert_eq!(mark.stream, serial_stream_code(SerialStream::UsjTx));
        assert_eq!(
            publisher.lines[1].slots[0].stream,
            serial_stream_code(SerialStream::Uart0Tx)
        );
    }

    #[test]
    fn events_are_mirrored_once_each_and_never_recopied() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        io.events.emit(HostEvent {
            kind: EventKind::Reset,
            vt: VTime(10),
            arg: 0x15,
        });
        publisher.refresh(&mut io, VTime(10));
        assert_eq!(publisher.events.published_head, 1);
        let first = publisher.events.slots[0];
        assert_eq!(first.kind, event_kind_code(EventKind::Reset));
        assert_eq!(first.vt_ps, 10);
        assert_eq!(first.arg, 0x15);
        assert_eq!(first.reserved, 0);

        io.events.emit(HostEvent {
            kind: EventKind::Panic,
            vt: VTime(20),
            arg: 1,
        });
        publisher.refresh(&mut io, VTime(20));
        assert_eq!(publisher.events.published_head, 2);
        assert_eq!(
            publisher.events.slots[1].kind,
            event_kind_code(EventKind::Panic)
        );
        assert_eq!(
            publisher.events.slots[0], first,
            "the first record was not rewritten"
        );
    }

    #[test]
    fn pcm_records_and_underflow_counters_reach_the_view() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        io.audio_out.write(VTime(500), 16_000, 1, &[1, 2, 3, 4]);
        publisher.refresh(&mut io, VTime(500));

        let cursors = publisher.cursors();
        assert_eq!(cursors[RingId::AudioOutSamples.head_slot() as usize], 4);
        assert_eq!(cursors[RingId::AudioOutRecords.head_slot() as usize], 1);
        let record = publisher.audio_out_records.slots[0];
        assert_eq!(record.vt_start_ps, 500);
        assert_eq!(record.fs, 16_000);
        assert_eq!(record.channels, 1);
        assert_eq!(record.first, 0);
        assert_eq!(cursors[SLOT_AUDIO_OUT_UNDERFLOWS as usize], 0);
        assert_eq!(cursors[SLOT_AUDIO_IN_UNDERFLOWS as usize], 0);
    }

    /// Every `audio_out` record reaches the mirror, not just the first. The guest changes the I2S
    /// format at runtime (`bsp_audio_set_format(16000, 16, 1)`), and the worklet learns it only
    /// from these records. A sample-cursor lookup answers record 0 for position 1 while sample 1 is
    /// kept, and `None` once evicted; both halves are asserted, in that order.
    #[test]
    fn a_later_pcm_record_reaches_the_view_with_its_own_format() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        // The boot stream: 16 kHz stereo, the format `official` opens I2S TX with.
        io.audio_out.write(VTime(0), 16_000, 2, &[1, 2, 3, 4]);
        publisher.refresh(&mut io, VTime(0));
        assert_eq!(publisher.audio_out_records.slots[0].channels, 2);

        // The format change, with sample 1 still in the ring: a sample-cursor lookup answers 0.
        io.audio_out.write(VTime(1_000), 16_000, 1, &[5, 6]);
        publisher.refresh(&mut io, VTime(1_000));
        let second = publisher.audio_out_records.slots[1];
        assert_eq!(
            (second.channels, second.first, second.vt_start_ps),
            (1, 4, 1_000),
            "the mono run is published as its own record, not as a copy of the boot record"
        );

        // The same change again, after the sample ring has evicted the low cursors: a
        // sample-cursor lookup answers `None` and leaves the slot at its zero value.
        let filler = vec![0i16; io.audio_out.capacity() + 16];
        io.audio_out.write(VTime(2_000), 16_000, 1, &filler);
        publisher.refresh(&mut io, VTime(2_000));
        assert!(
            io.audio_out.tail() > 2,
            "the filler evicted the samples the old lookup was asking about"
        );
        io.audio_out.write(VTime(3_000), 24_000, 2, &[7, 8, 9, 10]);
        publisher.refresh(&mut io, VTime(3_000));
        let head = publisher.cursors()[RingId::AudioOutRecords.head_slot() as usize];
        let newest = publisher.audio_out_records.slots
            [(head - 1) as usize % publisher.audio_out_records.capacity()];
        assert_eq!(
            (newest.fs, newest.channels),
            (24_000, 2),
            "a record written after the sample ring wrapped still reaches the view"
        );
    }

    #[test]
    fn the_frame_publishes_its_dirty_span_flags_and_generation() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        publisher.refresh(&mut io, VTime(0));
        assert_eq!(publisher.layout().frame_dirty_first, DIRTY_NONE);
        assert_eq!(publisher.layout().frame_flags & FRAME_FLAG_POWERED, 0);
        assert_ne!(publisher.layout().frame_flags & FRAME_FLAG_SLEEPING, 0);

        io.frame.set_powered(true);
        io.frame.set_sleeping(false);
        // The raw LEDC duty 8192 is 512 against `1 << 10`: half lit.
        io.frame.set_backlight(8192);
        io.frame.pixels_mut()[FRAME_WIDTH as usize * 3] = 0xF800;
        io.frame.mark_dirty(3, 3);
        publisher.refresh(&mut io, VTime(1));

        let layout = publisher.layout();
        assert_eq!(layout.frame_dirty_first, 3);
        assert_eq!(layout.frame_dirty_last, 3);
        assert_eq!(layout.frame_backlight, 512);
        assert_eq!(layout.frame_backlight_scale, DEFAULT_BACKLIGHT_SCALE);
        assert_ne!(layout.frame_flags & FRAME_FLAG_POWERED, 0);
        assert_eq!(layout.frame_flags & FRAME_FLAG_SLEEPING, 0);
        assert_eq!(
            publisher.cursors()[SLOT_FRAME_GENERATION as usize],
            io.frame.generation()
        );
    }

    /// A renderer draws the glass from the published flags, so they must follow the board's
    /// `invon_shows_ram` rule and DISPON rather than the bare INVON command state.
    #[test]
    fn the_glass_flags_follow_invon_shows_ram_and_dispon() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        let flags = |publisher: &IoPublisher| publisher.layout().frame_flags;

        // The official menu's init: INVON with the board default, so memory shows unmodified.
        io.frame.set_inverted(true);
        io.frame.set_display_on(true);
        publisher.refresh(&mut io, VTime(1));
        assert_ne!(flags(&publisher) & FRAME_FLAG_INVERTED, 0);
        assert_ne!(flags(&publisher) & FRAME_FLAG_DISPLAY_ON, 0);
        assert_eq!(flags(&publisher) & FRAME_FLAG_GLASS_COMPLEMENT, 0);

        io.frame.set_inverted(false);
        publisher.refresh(&mut io, VTime(2));
        assert_ne!(flags(&publisher) & FRAME_FLAG_GLASS_COMPLEMENT, 0);

        io.frame.set_invon_shows_ram(false);
        publisher.refresh(&mut io, VTime(3));
        assert_eq!(flags(&publisher) & FRAME_FLAG_GLASS_COMPLEMENT, 0);
        io.frame.set_inverted(true);
        publisher.refresh(&mut io, VTime(4));
        assert_ne!(flags(&publisher) & FRAME_FLAG_GLASS_COMPLEMENT, 0);

        io.frame.set_display_on(false);
        publisher.refresh(&mut io, VTime(5));
        assert_eq!(flags(&publisher) & FRAME_FLAG_DISPLAY_ON, 0);
    }

    #[test]
    fn the_backlight_numerator_is_the_shifted_duty_not_the_raw_ledc_register() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        io.frame.set_backlight(16_368);
        publisher.refresh(&mut io, VTime(0));
        assert_eq!(publisher.layout().frame_backlight, 1023);
        assert!(publisher.layout().frame_backlight < publisher.layout().frame_backlight_scale);
    }

    /// Without the take in `refresh` the span is the union of every frame ever painted, which
    /// after one full repaint is the whole panel.
    #[test]
    fn the_dirty_span_is_taken_so_the_next_refresh_publishes_only_the_new_rows() {
        let mut io = io();
        let mut publisher = IoPublisher::new(&io);
        io.frame.mark_dirty(0, 0);
        publisher.refresh(&mut io, VTime(1));
        assert_eq!(publisher.layout().frame_dirty_first, 0);
        assert_eq!(publisher.layout().frame_dirty_last, 0);
        assert_eq!(io.frame.dirty_rows(), None, "refresh is the acknowledge");

        io.frame.mark_dirty(100, 101);
        publisher.refresh(&mut io, VTime(2));
        assert_eq!(publisher.layout().frame_dirty_first, 100);
        assert_eq!(publisher.layout().frame_dirty_last, 101);

        publisher.refresh(&mut io, VTime(3));
        assert_eq!(
            publisher.layout().frame_dirty_first,
            DIRTY_NONE,
            "a slice that painted nothing publishes no span"
        );
    }
}
