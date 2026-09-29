//! Deterministic randomness: no host RNG reaches a state path. Every random value is ChaCha20
//! (RFC 8439) keyed by `MachineConfig.seed`, one stream per [`RngStream`] id, so adding a
//! consumer never shifts another's values.
//!
//! Stream `id` keys with `seed` (8 little-endian bytes) then `KEY_TAG`; state word 12 is the low
//! 32 bits of the block index and words 13 to 15 are `id`, the high 32 bits, then zero. The block
//! function is the RFC's, so its test vectors apply. A stream's state is `pos`, the words drawn;
//! it enters the snapshot on its first draw, not when a [`RngView`] is taken.

use serde::{Deserialize, Serialize};

/// Follows the seed in the key, so the key is not the bare seed.
const KEY_TAG: &[u8; 24] = b"pemu DetRng v1 key tag  ";

/// ChaCha20 constant words, "expand 32-byte k" (RFC 8439 section 2.3).
const CHACHA_CONST: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

const WORDS_PER_BLOCK: u64 = 16;

/// One ChaCha20 quarter round on four words of the working state (RFC 8439 section 2.1).
#[inline]
fn quarter_round(s: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(16);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(12);
    s[a] = s[a].wrapping_add(s[b]);
    s[d] = (s[d] ^ s[a]).rotate_left(8);
    s[c] = s[c].wrapping_add(s[d]);
    s[b] = (s[b] ^ s[c]).rotate_left(7);
}

/// The ChaCha20 block function of RFC 8439 section 2.3, as 16 little-endian keystream words.
fn chacha20_block(key: &[u8; 32], counter: u32, nonce: &[u8; 12]) -> [u32; 16] {
    let mut state = [0u32; 16];
    state[0..4].copy_from_slice(&CHACHA_CONST);
    for (i, chunk) in key.chunks_exact(4).enumerate() {
        state[4 + i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }
    state[12] = counter;
    for (i, chunk) in nonce.chunks_exact(4).enumerate() {
        state[13 + i] = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
    }

    let mut w = state;
    for _ in 0..10 {
        quarter_round(&mut w, 0, 4, 8, 12);
        quarter_round(&mut w, 1, 5, 9, 13);
        quarter_round(&mut w, 2, 6, 10, 14);
        quarter_round(&mut w, 3, 7, 11, 15);
        quarter_round(&mut w, 0, 5, 10, 15);
        quarter_round(&mut w, 1, 6, 11, 12);
        quarter_round(&mut w, 2, 7, 8, 13);
        quarter_round(&mut w, 3, 4, 9, 14);
    }
    for i in 0..16 {
        w[i] = w[i].wrapping_add(state[i]);
    }
    w
}

/// Consumer id selecting one [`DetRng`] stream. Ids are part of the run identity through the
/// values they produce: never reuse one, and add a new stream as a new constant.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub struct RngStream(pub u32);

impl RngStream {
    /// Guest entropy: APB_CTRL `RND_DATA` at 0x600260B0 (`specs/blocks/apb_ctrl.toml`).
    pub const GUEST_ENTROPY: RngStream = RngStream(0);

    /// Jitter for board models such as the ADC button ladder and the battery gauge.
    pub const BOARD_NOISE: RngStream = RngStream(1);

    /// Seed-derived identity bytes: eFuse unique id, placeholder MAC, NTAG213 UID and signature.
    pub const IDENTITY: RngStream = RngStream(2);

    /// The virtual LE controller's `HCI_LE_Rand`, drawn through `GuestView::draw_entropy`.
    pub const RADIO_BLE: RngStream = RngStream(3);
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
struct StreamPos {
    id: u32,
    pos: u64,
}

/// One keystream block, so 16 words cost one block call; never serialized.
#[derive(Copy, Clone, Debug)]
struct BlockCache {
    id: u32,
    index: u64,
    words: [u32; 16],
    valid: bool,
}

/// ChaCha20 keyed by `MachineConfig.seed`, one independent stream per consumer id.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(into = "RngState", from = "RngState")]
pub struct DetRng {
    seed: u64,
    key: [u8; 32],
    /// Sorted by id, so the snapshot bytes are canonical and a view can keep an index.
    streams: Vec<StreamPos>,
    cache: BlockCache,
}

/// Serialized form of [`DetRng`]; the key and the keystream cache are rebuilt from it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct RngState {
    seed: u64,
    streams: Vec<StreamPos>,
}

impl From<DetRng> for RngState {
    fn from(rng: DetRng) -> Self {
        RngState {
            seed: rng.seed,
            streams: rng.streams,
        }
    }
}

impl From<RngState> for DetRng {
    fn from(state: RngState) -> Self {
        let mut rng = DetRng::new(state.seed);
        rng.streams = state.streams;
        // Keep the lookup invariant (sorted, one entry per id) for a hand-written section.
        rng.streams.sort_unstable_by_key(|s| s.id);
        rng.streams.dedup_by_key(|s| s.id);
        // Position 0 is the same state as an absent stream; one spelling per state.
        rng.streams.retain(|s| s.pos != 0);
        rng
    }
}

impl DetRng {
    /// UNVERIFIED: a design choice.
    pub fn new(seed: u64) -> Self {
        let mut key = [0u8; 32];
        key[..8].copy_from_slice(&seed.to_le_bytes());
        key[8..].copy_from_slice(KEY_TAG);
        DetRng {
            seed,
            key,
            streams: Vec::new(),
            cache: BlockCache {
                id: 0,
                index: 0,
                words: [0; 16],
                valid: false,
            },
        }
    }

    /// Part of the run identity and of the snapshot header.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// View of stream `id`. Taking one records nothing, or every `Cx`'s view would reach
    /// `state_hash`; the first draw creates the stream.
    pub fn stream(&mut self, id: RngStream) -> RngView<'_> {
        let (idx, present) = match self.streams.binary_search_by_key(&id.0, |s| s.id) {
            Ok(i) => (i, true),
            Err(i) => (i, false),
        };
        RngView {
            rng: self,
            id: id.0,
            idx,
            present,
        }
    }

    /// Words drawn from stream `id` so far, without creating the stream.
    pub fn pos_of(&self, id: RngStream) -> u64 {
        match self.streams.binary_search_by_key(&id.0, |s| s.id) {
            Ok(i) => self.streams[i].pos,
            Err(_) => 0,
        }
    }

    /// The streams drawn from, in id order, with their positions.
    pub fn positions(&self) -> impl Iterator<Item = (RngStream, u64)> + '_ {
        self.streams.iter().map(|s| (RngStream(s.id), s.pos))
    }

    /// Word `pos` of stream `id`, from the cache when it holds that block.
    fn word_at(&mut self, id: u32, pos: u64) -> u32 {
        let index = pos / WORDS_PER_BLOCK;
        let word = (pos % WORDS_PER_BLOCK) as usize;
        if !self.cache.valid || self.cache.id != id || self.cache.index != index {
            let mut nonce = [0u8; 12];
            nonce[0..4].copy_from_slice(&id.to_le_bytes());
            nonce[4..8].copy_from_slice(&((index >> 32) as u32).to_le_bytes());
            let words = chacha20_block(&self.key, index as u32, &nonce);
            self.cache = BlockCache {
                id,
                index,
                words,
                valid: true,
            };
        }
        self.cache.words[word]
    }
}

/// Borrowed view of one [`DetRng`] stream, so a peripheral draws only from its own.
pub struct RngView<'a> {
    rng: &'a mut DetRng,
    id: u32,
    /// Index of this stream in `rng.streams`; stable while the view borrows exclusively.
    idx: usize,
    /// False until the first draw creates the stream.
    present: bool,
}

impl RngView<'_> {
    pub fn id(&self) -> RngStream {
        RngStream(self.id)
    }

    pub fn pos(&self) -> u64 {
        if self.present {
            self.rng.streams[self.idx].pos
        } else {
            0
        }
    }

    /// The stream's slot, creating it if this is its first draw.
    fn slot(&mut self) -> usize {
        if !self.present {
            self.rng.streams.insert(
                self.idx,
                StreamPos {
                    id: self.id,
                    pos: 0,
                },
            );
            self.present = true;
        }
        self.idx
    }

    pub fn next_u32(&mut self) -> u32 {
        let i = self.slot();
        let entry = self.rng.streams[i];
        let v = self.rng.word_at(entry.id, entry.pos);
        self.rng.streams[i].pos = entry.pos + 1;
        v
    }

    /// Two words, the first the low half.
    pub fn next_u64(&mut self) -> u64 {
        let lo = u64::from(self.next_u32());
        let hi = u64::from(self.next_u32());
        lo | (hi << 32)
    }

    /// Fills `out` with little-endian words; a partial last word's unused bytes are discarded, so
    /// the words consumed depend only on the length.
    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(4) {
            let word = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RFC 8439 section 2.3.1 serialization.
    fn block_bytes(words: &[u32; 16]) -> [u8; 64] {
        let mut out = [0u8; 64];
        for (i, w) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    fn hex(bytes: &[u8]) -> String {
        let mut s = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn chacha20_block_matches_rfc_8439_2_3_2() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 9, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let got = block_bytes(&chacha20_block(&key, 1, &nonce));
        assert_eq!(
            hex(&got),
            "10f1e7e4d13b5915500fdd1fa32071c4\
             c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2\
             b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    /// Counter 2 checks that a stream continues across a block boundary.
    #[test]
    fn chacha20_block_matches_rfc_8439_2_4_2() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = i as u8;
        }
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        assert_eq!(
            hex(&block_bytes(&chacha20_block(&key, 1, &nonce))),
            "224f51f3401bd9e12fde276fb8631ded\
             8c131f823d2c06e27e4fcaec9ef3cf78\
             8a3b0aa372600a92b57974cded2b9334\
             794cba40c63e34cdea212c4cf07d41b7"
        );
        assert_eq!(
            hex(&block_bytes(&chacha20_block(&key, 2, &nonce))),
            "69a6749f3f630f4122cafe28ec4dc47e\
             26d4346d70b98c73f3e9c53ac40c5945\
             398b6eda1a832c89c167eacd901d7e2b\
             f363740373201aa188fbbce83991c4ed"
        );
    }

    #[test]
    fn key_derivation_is_the_documented_one() {
        assert_eq!(KEY_TAG.len(), 24);
        let rng = DetRng::new(0x0123_4567_89ab_cdef);
        assert_eq!(&rng.key[..8], &0x0123_4567_89ab_cdef_u64.to_le_bytes());
        assert_eq!(&rng.key[8..], &KEY_TAG[..]);
        assert_eq!(rng.seed(), 0x0123_4567_89ab_cdef);
    }

    /// Pins the stream construction, so a change fails here and not in a boot golden.
    #[test]
    fn stream_words_are_the_documented_construction() {
        let mut rng = DetRng::new(0);
        let mut s0: Vec<u32> = Vec::new();
        for _ in 0..20 {
            s0.push(rng.stream(RngStream::GUEST_ENTROPY).next_u32());
        }
        assert_eq!(
            s0[..8],
            [
                0x9369_01c7,
                0x387e_b967,
                0x4088_2738,
                0xb3af_8710,
                0x7138_bd5f,
                0x3139_9723,
                0x9683_5e28,
                0x0114_a33f
            ]
        );
        // Words 14 to 19 cross the block boundary.
        assert_eq!(
            s0[14..20],
            [
                0x5c11_2b21,
                0x6f92_7ed7,
                0x60fa_2178,
                0x5844_2f10,
                0xc4f7_5c1f,
                0xed4f_9fdd
            ]
        );

        let mut s1: Vec<u32> = Vec::new();
        for _ in 0..4 {
            s1.push(rng.stream(RngStream::BOARD_NOISE).next_u32());
        }
        assert_eq!(s1, [0x29d2_bd3d, 0xf352_1253, 0x67ae_2269, 0x2035_6e02]);

        let mut other = DetRng::new(0x0123_4567_89ab_cdef);
        let mut s2: Vec<u32> = Vec::new();
        for _ in 0..4 {
            s2.push(other.stream(RngStream::GUEST_ENTROPY).next_u32());
        }
        assert_eq!(s2, [0x125d_6c77, 0xff62_83ae, 0xefeb_0801, 0x2734_3365]);
    }

    #[test]
    fn streams_are_independent() {
        let ids = [
            RngStream::GUEST_ENTROPY,
            RngStream::BOARD_NOISE,
            RngStream::IDENTITY,
            RngStream(9),
        ];

        let mut alone: Vec<Vec<u32>> = Vec::new();
        for id in ids {
            let mut rng = DetRng::new(42);
            let mut v = rng.stream(id);
            alone.push((0..8).map(|_| v.next_u32()).collect());
        }

        // Interleaved, in another order, with extra draws in between.
        let mut rng = DetRng::new(42);
        let mut got: Vec<Vec<u32>> = vec![Vec::new(); ids.len()];
        for round in 0..8 {
            for (i, id) in ids.iter().enumerate().rev() {
                got[i].push(rng.stream(*id).next_u32());
            }
            let _ = rng.stream(RngStream(1000 + round)).next_u64();
        }
        assert_eq!(got, alone);

        for id in ids {
            assert_eq!(rng.pos_of(id), 8);
        }
        assert_eq!(rng.pos_of(RngStream(12345)), 0);
        let positions: Vec<u32> = rng.positions().map(|(id, _)| id.0).collect();
        let mut sorted = positions.clone();
        sorted.sort_unstable();
        assert_eq!(positions, sorted, "positions are canonical, in id order");
    }

    #[test]
    fn the_seed_selects_the_sequence() {
        let draw = |seed| {
            let mut rng = DetRng::new(seed);
            let mut v = rng.stream(RngStream::GUEST_ENTROPY);
            (0..4).map(|_| v.next_u32()).collect::<Vec<_>>()
        };
        assert_eq!(draw(7), draw(7));
        assert_ne!(draw(7), draw(8));
        assert_ne!(draw(0), draw(u64::MAX));
    }

    #[test]
    fn wider_draws_compose_from_words() {
        let mut a = DetRng::new(3);
        let (lo, hi) = {
            let mut v = a.stream(RngStream::IDENTITY);
            (v.next_u32(), v.next_u32())
        };
        let mut b = DetRng::new(3);
        assert_eq!(
            b.stream(RngStream::IDENTITY).next_u64(),
            u64::from(lo) | (u64::from(hi) << 32)
        );

        let mut c = DetRng::new(3);
        let mut bytes = [0u8; 7];
        c.stream(RngStream::IDENTITY).fill_bytes(&mut bytes);
        assert_eq!(bytes[..4], lo.to_le_bytes());
        assert_eq!(bytes[4..7], hi.to_le_bytes()[..3]);
        assert_eq!(c.pos_of(RngStream::IDENTITY), 2);

        let mut d = DetRng::new(3);
        d.stream(RngStream::IDENTITY).fill_bytes(&mut []);
        assert_eq!(d.pos_of(RngStream::IDENTITY), 0);
    }

    #[test]
    fn a_serde_round_trip_continues_every_stream() {
        let mut rng = DetRng::new(0xdead_beef);
        for _ in 0..17 {
            rng.stream(RngStream::GUEST_ENTROPY).next_u32();
        }
        for _ in 0..3 {
            rng.stream(RngStream::BOARD_NOISE).next_u32();
        }

        let bytes = postcard::to_allocvec(&rng).expect("rng serializes");
        let mut restored: DetRng = postcard::from_bytes(&bytes).expect("rng deserializes");
        assert_eq!(restored.seed(), rng.seed());
        assert_eq!(
            restored.positions().collect::<Vec<_>>(),
            rng.positions().collect::<Vec<_>>()
        );
        assert_eq!(restored.key, rng.key, "the key is rebuilt from the seed");

        for id in [RngStream::GUEST_ENTROPY, RngStream::BOARD_NOISE] {
            let mut want = Vec::new();
            let mut got = Vec::new();
            for _ in 0..20 {
                want.push(rng.stream(id).next_u32());
                got.push(restored.stream(id).next_u32());
            }
            assert_eq!(got, want);
        }

        // Canonical bytes, whatever order the streams were first touched in.
        let mut other = DetRng::new(0xdead_beef);
        for _ in 0..3 {
            other.stream(RngStream::BOARD_NOISE).next_u32();
        }
        for _ in 0..17 {
            other.stream(RngStream::GUEST_ENTROPY).next_u32();
        }
        assert_eq!(postcard::to_allocvec(&other).expect("serializes"), bytes);
    }

    #[test]
    fn stream_returns_a_view_of_the_requested_stream() {
        let mut rng = DetRng::new(7);
        assert_eq!(rng.stream(RngStream(3)).id(), RngStream(3));
        assert_eq!(rng.stream(RngStream(0)).id(), RngStream(0));
        let mut v = rng.stream(RngStream(3));
        assert_eq!(v.pos(), 0);
        v.next_u32();
        assert_eq!(v.pos(), 1);
    }

    #[test]
    fn taking_a_view_leaves_no_trace_in_the_section() {
        let untouched = postcard::to_allocvec(&DetRng::new(1)).expect("rng serializes");

        let mut viewed = DetRng::new(1);
        for id in [
            RngStream::GUEST_ENTROPY,
            RngStream::BOARD_NOISE,
            RngStream::IDENTITY,
        ] {
            let v = viewed.stream(id);
            assert_eq!(v.id(), id);
            assert_eq!(v.pos(), 0);
        }
        // A view that asks for nothing, and a fill of zero bytes, are not draws either.
        viewed.stream(RngStream::IDENTITY).fill_bytes(&mut []);
        assert_eq!(viewed.positions().count(), 0);
        assert_eq!(viewed.pos_of(RngStream::GUEST_ENTROPY), 0);
        assert_eq!(
            postcard::to_allocvec(&viewed).expect("rng serializes"),
            untouched,
            "an untouched stream is indistinguishable from an absent one"
        );

        viewed.stream(RngStream::BOARD_NOISE).next_u32();
        assert_eq!(
            viewed.positions().collect::<Vec<_>>(),
            [(RngStream::BOARD_NOISE, 1)]
        );
        assert_ne!(
            postcard::to_allocvec(&viewed).expect("rng serializes"),
            untouched
        );

        // A section carrying position-0 streams normalizes to the same state.
        let padded: DetRng = postcard::from_bytes(
            &postcard::to_allocvec(&RngState {
                seed: 1,
                streams: vec![
                    StreamPos { id: 0, pos: 0 },
                    StreamPos { id: 1, pos: 1 },
                    StreamPos { id: 9, pos: 0 },
                ],
            })
            .expect("state serializes"),
        )
        .expect("rng deserializes");
        assert_eq!(
            postcard::to_allocvec(&padded).expect("rng serializes"),
            postcard::to_allocvec(&viewed).expect("rng serializes")
        );
    }
}
