//! The ring storage behind every `HostIo` channel: a fixed buffer windowed by absolute `u64`
//! cursors, as the `hostio` module documentation describes.

use core::fmt;

/// Byte ring with absolute cursors and a fixed capacity. Guest-to-host rings take
/// [`ByteRing::write`], which evicts the oldest bytes so the guest never blocks on a slow reader;
/// host-to-guest transport rings take [`ByteRing::push`], which never evicts, and are drained with
/// [`ByteRing::pop`].
#[derive(Clone)]
pub struct ByteRing {
    buf: Box<[u8]>,
    head: u64,
    tail: u64,
}

/// Result of a copying read from an absolute cursor ([`ByteRing::read`],
/// [`LineIndex::read`](super::LineIndex::read)).
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub struct RingRead {
    pub n: usize,
    /// Cursor for the next read: the position after the last item copied.
    pub next: u64,
    /// Items the reader lost: how far the requested cursor fell behind `tail`, else 0.
    pub dropped: u64,
}

/// Zero-copy view of a ring window from an absolute cursor ([`ByteRing::slices`],
/// [`LineIndex::slices`](super::LineIndex::slices)). The items are `first` followed by `second`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct RingSlices<'a, T> {
    pub dropped: u64,
    pub start: u64,
    pub first: &'a [T],
    /// Non-empty only when the window wraps.
    pub second: &'a [T],
    /// The ring's `head`.
    pub next: u64,
}

impl<T> RingSlices<'_, T> {
    pub fn len(&self) -> usize {
        self.first.len() + self.second.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.first.iter().chain(self.second)
    }
}

/// Why a ring restore refused its input.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RingRestoreError {
    /// More items than the ring's fixed capacity: restore never reallocates.
    TooLong { len: usize, capacity: usize },
    /// `tail + len` does not fit in a `u64` cursor.
    CursorOverflow,
}

impl fmt::Display for RingRestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingRestoreError::TooLong { len, capacity } => {
                write!(f, "ring contents of {len} items exceed capacity {capacity}")
            }
            RingRestoreError::CursorOverflow => f.write_str("ring cursor overflows u64"),
        }
    }
}

impl std::error::Error for RingRestoreError {}

impl ByteRing {
    /// A zero capacity is allowed: such a ring keeps nothing, but its cursors still count.
    pub fn new(capacity: usize) -> Self {
        ByteRing {
            buf: vec![0; capacity].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Total bytes ever written.
    pub fn head(&self) -> u64 {
        self.head
    }

    pub fn tail(&self) -> u64 {
        self.tail
    }

    pub fn len(&self) -> usize {
        window_len(self.head, self.tail)
    }

    pub fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    pub fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    /// Address of the backing buffer; it never changes for the life of the ring.
    pub fn as_ptr(&self) -> *const u8 {
        self.buf.as_ptr()
    }

    /// Appends `bytes`, evicting the oldest when full. Every byte gets a cursor, including bytes
    /// evicted in the same call.
    pub fn write(&mut self, bytes: &[u8]) {
        ring_overwrite(&mut self.buf, &mut self.head, &mut self.tail, bytes);
    }

    /// Appends as many of `bytes` as fit without evicting and returns that count.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        ring_push(&mut self.buf, &mut self.head, self.tail, bytes)
    }

    /// Removes up to `out.len()` of the oldest bytes into `out` and returns the count. Readers
    /// whose cursor was below the new `tail` see those bytes as dropped.
    pub fn pop(&mut self, out: &mut [u8]) -> usize {
        let r = self.read(self.tail, out);
        self.tail = r.next;
        r.n
    }

    /// Copies bytes from `cursor` into `out` without consuming them. A cursor below `tail` reports
    /// the gap in [`RingRead::dropped`] and reads from `tail`; a cursor above `head` reads as
    /// `head` (UNVERIFIED choice, for example after a restore to an earlier state).
    pub fn read(&self, cursor: u64, out: &mut [u8]) -> RingRead {
        ring_read(&self.buf, self.head, self.tail, cursor, out)
    }

    /// Zero-copy view from `cursor` to `head`, with the cursor rules of [`ByteRing::read`].
    pub fn slices(&self, cursor: u64) -> RingSlices<'_, u8> {
        ring_slices(&self.buf, self.head, self.tail, cursor)
    }

    /// Replaces the window with `bytes` starting at cursor `tail`, in place. Allocates nothing.
    pub fn restore(&mut self, tail: u64, bytes: &[u8]) -> Result<(), RingRestoreError> {
        ring_restore(&mut self.buf, &mut self.head, &mut self.tail, tail, bytes)
    }
}

impl PartialEq for ByteRing {
    /// Equal when capacity, cursors and window bytes are equal; stale bytes outside the window do
    /// not count.
    fn eq(&self, other: &Self) -> bool {
        self.capacity() == other.capacity()
            && self.head == other.head
            && self.tail == other.tail
            && self
                .slices(self.tail)
                .iter()
                .eq(other.slices(other.tail).iter())
    }
}

impl Eq for ByteRing {}

impl fmt::Debug for ByteRing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteRing")
            .field("capacity", &self.capacity())
            .field("head", &self.head)
            .field("tail", &self.tail)
            .finish()
    }
}

/// Fixed-capacity ring of `Copy` items windowed by absolute cursors `[tail, head)`: the storage of
/// [`LineIndex`], [`PcmRing`] and [`EventRing`].
#[derive(Clone, Default)]
pub(super) struct Ring<T> {
    buf: Box<[T]>,
    pub(super) head: u64,
    pub(super) tail: u64,
}

impl<T: Copy + Default> Ring<T> {
    pub(super) fn new(capacity: usize) -> Self {
        Ring {
            buf: vec![T::default(); capacity].into_boxed_slice(),
            head: 0,
            tail: 0,
        }
    }
}

impl<T: Copy> Ring<T> {
    pub(super) fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Fixed for the life of the ring.
    pub(super) fn as_ptr(&self) -> *const T {
        self.buf.as_ptr()
    }

    pub(super) fn len(&self) -> usize {
        window_len(self.head, self.tail)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.head == self.tail
    }

    pub(super) fn free(&self) -> usize {
        self.capacity() - self.len()
    }

    pub(super) fn last(&self) -> Option<T> {
        if self.is_empty() {
            return None;
        }
        self.slices(self.head - 1).first.first().copied()
    }

    pub(super) fn write(&mut self, items: &[T]) {
        ring_overwrite(&mut self.buf, &mut self.head, &mut self.tail, items);
    }

    pub(super) fn push(&mut self, items: &[T]) -> usize {
        ring_push(&mut self.buf, &mut self.head, self.tail, items)
    }

    pub(super) fn pop(&mut self, out: &mut [T]) -> usize {
        let r = self.read(self.tail, out);
        self.tail = r.next;
        r.n
    }

    pub(super) fn read(&self, cursor: u64, out: &mut [T]) -> RingRead {
        ring_read(&self.buf, self.head, self.tail, cursor, out)
    }

    pub(super) fn slices(&self, cursor: u64) -> RingSlices<'_, T> {
        ring_slices(&self.buf, self.head, self.tail, cursor)
    }

    pub(super) fn restore(&mut self, tail: u64, items: &[T]) -> Result<(), RingRestoreError> {
        ring_restore(&mut self.buf, &mut self.head, &mut self.tail, tail, items)
    }
}

impl<T: Copy + PartialEq> PartialEq for Ring<T> {
    fn eq(&self, other: &Self) -> bool {
        self.capacity() == other.capacity()
            && self.head == other.head
            && self.tail == other.tail
            && self
                .slices(self.tail)
                .iter()
                .eq(other.slices(other.tail).iter())
    }
}

impl<T: Copy + Eq> Eq for Ring<T> {}

impl<T> fmt::Debug for Ring<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ring")
            .field("capacity", &self.buf.len())
            .field("head", &self.head)
            .field("tail", &self.tail)
            .finish()
    }
}

/// Items in the window `[tail, head)`; never more than the capacity, so it fits a `usize`.
fn window_len(head: u64, tail: u64) -> usize {
    (head - tail) as usize
}

fn ring_slot(pos: u64, cap: usize) -> usize {
    (pos % cap as u64) as usize
}

/// Stores `src` (at most `buf.len()` items) so that item `i` lands in the slot of `pos + i`.
fn ring_copy_in<T: Copy>(buf: &mut [T], pos: u64, src: &[T]) {
    debug_assert!(src.len() <= buf.len());
    if src.is_empty() {
        return;
    }
    let start = ring_slot(pos, buf.len());
    let first = src.len().min(buf.len() - start);
    buf[start..start + first].copy_from_slice(&src[..first]);
    buf[..src.len() - first].copy_from_slice(&src[first..]);
}

fn ring_overwrite<T: Copy>(buf: &mut [T], head: &mut u64, tail: &mut u64, src: &[T]) {
    let keep = src.len().min(buf.len());
    let skip = src.len() - keep;
    ring_copy_in(buf, *head + skip as u64, &src[skip..]);
    *head += src.len() as u64;
    *tail = (*tail).max(head.saturating_sub(buf.len() as u64));
}

fn ring_push<T: Copy>(buf: &mut [T], head: &mut u64, tail: u64, src: &[T]) -> usize {
    let n = src.len().min(buf.len() - window_len(*head, tail));
    ring_copy_in(buf, *head, &src[..n]);
    *head += n as u64;
    n
}

/// The head of a window of `len` items from `tail` in a ring of `capacity` items, or why such a
/// window cannot be restored.
pub(super) fn ring_check(capacity: usize, tail: u64, len: usize) -> Result<u64, RingRestoreError> {
    if len > capacity {
        return Err(RingRestoreError::TooLong { len, capacity });
    }
    tail.checked_add(len as u64)
        .ok_or(RingRestoreError::CursorOverflow)
}

/// On error nothing changes.
fn ring_restore<T: Copy>(
    buf: &mut [T],
    head: &mut u64,
    tail: &mut u64,
    new_tail: u64,
    src: &[T],
) -> Result<(), RingRestoreError> {
    let new_head = ring_check(buf.len(), new_tail, src.len())?;
    ring_copy_in(buf, new_tail, src);
    *tail = new_tail;
    *head = new_head;
    Ok(())
}

/// The window from `cursor`, clamped into `[tail, head]`, as two ordered slices.
fn ring_slices<T>(buf: &[T], head: u64, tail: u64, cursor: u64) -> RingSlices<'_, T> {
    let (start, dropped) = if cursor < tail {
        (tail, tail - cursor)
    } else {
        (cursor.min(head), 0)
    };
    let len = window_len(head, start);
    let (first, second) = if len == 0 {
        (&buf[..0], &buf[..0])
    } else {
        let slot = ring_slot(start, buf.len());
        let first = len.min(buf.len() - slot);
        (&buf[slot..slot + first], &buf[..len - first])
    };
    RingSlices {
        dropped,
        start,
        first,
        second,
        next: head,
    }
}

/// Copies the window from `cursor` into `out`, as much as fits.
fn ring_read<T: Copy>(buf: &[T], head: u64, tail: u64, cursor: u64, out: &mut [T]) -> RingRead {
    let s = ring_slices(buf, head, tail, cursor);
    let a = s.first.len().min(out.len());
    out[..a].copy_from_slice(&s.first[..a]);
    let b = s.second.len().min(out.len() - a);
    out[a..a + b].copy_from_slice(&s.second[..b]);
    RingRead {
        n: a + b,
        next: s.start + (a + b) as u64,
        dropped: s.dropped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hostio::tests::window;

    #[test]
    fn write_and_read_with_cursors() {
        let mut ring = ByteRing::new(8);
        ring.write(b"abc");
        let mut out = [0u8; 8];
        let r = ring.read(0, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 3,
                next: 3,
                dropped: 0
            }
        );
        assert_eq!(&out[..r.n], b"abc");

        ring.write(b"de");
        let r = ring.read(r.next, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 2,
                next: 5,
                dropped: 0
            }
        );
        assert_eq!(&out[..r.n], b"de");

        let r = ring.read(r.next, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 0,
                next: 5,
                dropped: 0
            }
        );

        // A short output slice stops early; the next read resumes exactly there.
        let mut two = [0u8; 2];
        let r = ring.read(1, &mut two);
        assert_eq!((r.n, r.next, &two), (2, 3, b"bc"));
        assert_eq!(
            ring.read(1, &mut []),
            RingRead {
                n: 0,
                next: 1,
                dropped: 0
            }
        );

        // A cursor ahead of head reads as head.
        assert_eq!(
            ring.read(99, &mut out),
            RingRead {
                n: 0,
                next: 5,
                dropped: 0
            }
        );
        assert_eq!(ring.len(), 5);
    }

    #[test]
    fn wraparound_keeps_order_across_the_buffer_end() {
        let mut ring = ByteRing::new(4);
        ring.write(b"abc");
        ring.write(b"def");
        assert_eq!((ring.tail(), ring.head(), ring.len()), (2, 6, 4));
        let s = ring.slices(2);
        assert_eq!((s.start, s.next, s.dropped), (2, 6, 0));
        assert_eq!((s.first, s.second), (&b"cd"[..], &b"ef"[..]));

        let mut out = [0u8; 4];
        let r = ring.read(3, &mut out);
        assert_eq!((r.n, r.next, &out[..r.n]), (3, 6, &b"def"[..]));

        for chunk in [&b"gh"[..], b"i", b"jklm"] {
            ring.write(chunk);
        }
        assert_eq!((ring.tail(), ring.head()), (9, 13));
        assert_eq!(window(&ring, 0), b"jklm");
    }

    #[test]
    fn reader_behind_tail_gets_dropped_count_and_oldest_kept_byte() {
        let mut ring = ByteRing::new(4);
        for chunk in [&b"012"[..], b"3456", b"789"] {
            ring.write(chunk);
        }
        assert_eq!((ring.tail(), ring.head()), (6, 10));

        let mut out = [0u8; 16];
        let r = ring.read(1, &mut out);
        assert_eq!(
            r,
            RingRead {
                n: 4,
                next: 10,
                dropped: 5
            }
        );
        assert_eq!(out[0], b'6');
        assert_eq!(&out[..r.n], b"6789");

        // A partial read behind the tail still starts at the oldest kept byte.
        let mut one = [0u8; 1];
        let r = ring.read(0, &mut one);
        assert_eq!(
            (r, one[0]),
            (
                RingRead {
                    n: 1,
                    next: 7,
                    dropped: 6
                },
                b'6'
            )
        );
        // Continuing from `next` loses nothing more.
        assert_eq!(ring.read(r.next, &mut out).dropped, 0);

        let s = ring.slices(3);
        assert_eq!((s.dropped, s.start, s.len()), (3, 6, 4));
    }

    #[test]
    fn independent_readers_do_not_disturb_each_other() {
        let mut ring = ByteRing::new(16);
        let (mut slow, mut fast) = (0u64, 0u64);
        let (mut got_slow, mut got_fast) = (Vec::new(), Vec::new());
        let mut one = [0u8; 1];
        let mut all = [0u8; 16];
        for chunk in [&b"hello "[..], b"wor", b"ld\n"] {
            ring.write(chunk);
            let before = ring.clone();
            let r = ring.read(slow, &mut one);
            got_slow.extend_from_slice(&one[..r.n]);
            slow = r.next;
            let r = ring.read(fast, &mut all);
            got_fast.extend_from_slice(&all[..r.n]);
            fast = r.next;
            assert_eq!(ring, before);
        }
        assert_eq!(got_fast, b"hello world\n");
        assert_eq!((slow, fast), (3, 12));
        while slow < ring.head() {
            let r = ring.read(slow, &mut one);
            assert_eq!(r.dropped, 0);
            got_slow.extend_from_slice(&one[..r.n]);
            slow = r.next;
        }
        assert_eq!(got_slow, got_fast);
    }

    #[test]
    fn capacity_edge_cases() {
        // Zero capacity keeps nothing but still counts cursors.
        let mut zero = ByteRing::new(0);
        zero.write(b"abc");
        assert_eq!(
            (zero.tail(), zero.head(), zero.len(), zero.free()),
            (3, 3, 0, 0)
        );
        assert_eq!(
            zero.read(1, &mut [0; 4]),
            RingRead {
                n: 0,
                next: 3,
                dropped: 2
            }
        );
        assert_eq!((zero.push(b"x"), zero.pop(&mut [0; 4])), (0, 0));
        assert!(zero.slices(0).is_empty());

        // Capacity one keeps the newest byte.
        let mut one = ByteRing::new(1);
        one.write(b"xyz");
        assert_eq!(
            (one.tail(), one.head(), window(&one, 0)),
            (2, 3, b"z".to_vec())
        );

        // Exactly capacity fills without eviction; one more evicts one.
        let mut ring = ByteRing::new(4);
        ring.write(b"abcd");
        assert_eq!((ring.tail(), ring.head(), ring.free()), (0, 4, 0));
        assert_eq!(window(&ring, 0), b"abcd");
        ring.write(b"e");
        assert_eq!((ring.tail(), window(&ring, 0)), (1, b"bcde".to_vec()));

        // One write longer than the capacity keeps its newest bytes at their own cursors.
        let mut big = ByteRing::new(4);
        big.write(b"ab");
        big.write(b"0123456789");
        assert_eq!((big.tail(), big.head()), (8, 12));
        assert_eq!(window(&big, 0), b"6789");
        big.write(b"!");
        assert_eq!(window(&big, 9), b"789!");
    }

    #[test]
    fn push_never_evicts_and_pop_drains_the_oldest_bytes() {
        let mut ring = ByteRing::new(4);
        assert_eq!(ring.push(b"abcdef"), 4);
        assert_eq!(ring.push(b"x"), 0);
        let mut out = [0u8; 2];
        assert_eq!((ring.pop(&mut out), &out), (2, b"ab"));
        assert_eq!(ring.push(b"xyz"), 2);
        let mut rest = [0u8; 8];
        let n = ring.pop(&mut rest);
        assert_eq!(&rest[..n], b"cdxy");
        assert!(ring.is_empty());
        assert_eq!((ring.tail(), ring.head()), (6, 6));
        // A cursor reader behind the drained bytes sees them as dropped.
        assert_eq!(
            ring.read(0, &mut rest),
            RingRead {
                n: 0,
                next: 6,
                dropped: 6
            }
        );
        assert_eq!(ring.pop(&mut rest), 0);
    }

    #[test]
    fn identical_writes_give_identical_cursors() {
        let stream: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let chunks = [7usize, 1, 64, 3, 300, 0, 25];
        let feed = |cap: usize, sizes: &[usize]| {
            let mut ring = ByteRing::new(cap);
            let mut rest = &stream[..];
            let mut sizes = sizes.iter().cycle();
            while !rest.is_empty() {
                let n = (*sizes.next().unwrap()).min(rest.len());
                ring.write(&rest[..n]);
                rest = &rest[n..];
            }
            ring
        };
        let a = feed(100, &chunks);
        let b = feed(100, &chunks);
        assert_eq!((a.head(), a.tail()), (b.head(), b.tail()));
        assert_eq!(a, b);
        // A different chunking of the same bytes gives the same cursors and window.
        let c = feed(100, &[1]);
        assert_eq!((c.head(), c.tail()), (1000, 900));
        assert_eq!(a, c);
        assert_eq!(window(&a, 0), &stream[900..]);
    }

    #[test]
    fn restore_is_in_place_and_the_buffer_never_moves() {
        let mut ring = ByteRing::new(8);
        let ptr = ring.as_ptr();
        for i in 0..100u8 {
            ring.write(&[i; 5]);
            ring.push(&[i; 3]);
            ring.pop(&mut [0; 2]);
        }
        assert_eq!(ring.as_ptr(), ptr);

        assert_eq!(ring.restore(100, b"xyz"), Ok(()));
        assert_eq!(
            (ring.tail(), ring.head(), window(&ring, 0)),
            (100, 103, b"xyz".to_vec())
        );
        assert_eq!(ring.as_ptr(), ptr);
        let mut fresh = ByteRing::new(8);
        fresh.restore(100, b"xyz").unwrap();
        assert_eq!(ring, fresh);

        assert_eq!(
            ring.restore(0, &[0; 9]),
            Err(RingRestoreError::TooLong {
                len: 9,
                capacity: 8
            })
        );
        assert_eq!(
            ring.restore(u64::MAX, b"a"),
            Err(RingRestoreError::CursorOverflow)
        );
        assert_eq!((ring.tail(), ring.head()), (100, 103));
    }

    #[test]
    fn reads_match_a_model_of_every_byte_written() {
        let mut ring = ByteRing::new(13);
        let mut model: Vec<u8> = Vec::new();
        let mut x = 0x2545_f491_u32; // fixed-seed xorshift, no host entropy
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut out = [0u8; 9];
        for _ in 0..2000 {
            let n = (next() % 20) as usize;
            let bytes: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            ring.write(&bytes);
            model.extend_from_slice(&bytes);
            let head = model.len() as u64;
            let tail = head.saturating_sub(13);
            assert_eq!((ring.head(), ring.tail()), (head, tail));
            let cursor = u64::from(next()) % (head + 1);
            let r = ring.read(cursor, &mut out);
            let start = cursor.max(tail) as usize;
            let expect = &model[start..model.len().min(start + out.len())];
            assert_eq!(&out[..r.n], expect);
            assert_eq!(r.dropped, tail.saturating_sub(cursor));
            assert_eq!(r.next, start as u64 + r.n as u64);
        }
    }
}
