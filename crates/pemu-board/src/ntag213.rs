//! The NTAG213 card and the tap API: memory, the eight commands, and the NDEF helpers (NTAG213
//! datasheet, NFC Forum Type 2 Tag and NDEF specifications).
//!
//! The card has no MCU connection; the firmware never touches it. It lives in [`BoardWorld`] in
//! the `card` domain, which only `nfc.reset()` clears: power cycles, brownouts and flash erases do
//! not reach a passive tag. UNVERIFIED and marked where defined: the page 2 internal byte, the
//! CFG1 RFUI defaults and the READ_CNT byte order.

use pemu_core::rng::{DetRng, RngStream};
use serde::{Deserialize, Serialize};

use crate::traits::BoardDomain;

pub const PAGES: usize = 45;
pub const PAGE_BYTES: usize = 4;
pub const MEM_BYTES: usize = PAGES * PAGE_BYTES;
pub const USER_FIRST: u8 = 0x04;
/// Last page of user memory; 144 bytes in total, which is what CC byte 2 = 0x12 means.
pub const USER_LAST: u8 = 0x27;
pub const PAGE_DYN_LOCK: u8 = 0x28;
/// Page of CFG0: MIRROR, RFUI, MIRROR_PAGE, AUTH0.
pub const PAGE_CFG0: u8 = 0x29;
/// Page of CFG1: ACCESS and three RFUI bytes.
pub const PAGE_CFG1: u8 = 0x2A;
pub const PAGE_PWD: u8 = 0x2B;
/// Page of PACK plus two RFUI bytes.
pub const PAGE_PACK: u8 = 0x2C;
pub const PAGE_MAX: u8 = PAGE_PACK;
pub const ACK: u8 = 0xA;
pub const VERSION: [u8; 8] = [0x00, 0x04, 0x04, 0x02, 0x01, 0x00, 0x0F, 0x03];
/// Highest value the 24-bit NFC counter reaches; it saturates there.
pub const COUNTER_MAX: u32 = 0xFF_FFFF;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Nak {
    /// 0h: invalid argument, an out-of-range or protected address.
    InvalidArgument,
    /// 1h: parity or CRC error. Never raised here; CRC_A lives below this layer.
    CrcError,
    /// 4h: the authentication counter overflowed (AUTHLIM reached).
    AuthOverflow,
    /// 5h: EEPROM write error, here a write to a locked page.
    WriteError,
}

impl Nak {
    pub const fn code(self) -> u8 {
        match self {
            Nak::InvalidArgument => 0x0,
            Nak::CrcError => 0x1,
            Nak::AuthOverflow => 0x4,
            Nak::WriteError => 0x5,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CardResponse {
    Data(Vec<u8>),
    Ack,
    Nak(Nak),
}

impl CardResponse {
    /// The response as a raw tap reports it; ACK and NAK as one byte.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            CardResponse::Data(data) => data.clone(),
            CardResponse::Ack => vec![ACK],
            CardResponse::Nak(nak) => vec![nak.code()],
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Ntag213 {
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, MEM_BYTES>")]
    mem: Vec<u8>,
    /// Originality signature, synthesized from the machine seed; deliberately not NXP's (class C).
    #[serde(deserialize_with = "pemu_core::snap::exact_vec::<_, _, 32>")]
    signature: Vec<u8>,
    counter: u32,
    in_field: bool,
    /// Whether the counter may still increment: only the first read after field-on does.
    counter_armed: bool,
    authenticated: bool,
    /// Failed PWD_AUTH attempts since the last success; tag state, it survives the field dropping.
    auth_fails: u8,
    /// CFGLCK as latched at the last field-on, the tag's power cycle.
    cfg_locked: bool,
}

impl Default for Ntag213 {
    /// The factory delivery state with a fixed all-zero-seed UID.
    fn default() -> Self {
        Ntag213::new(&mut DetRng::new(0))
    }
}

impl Ntag213 {
    /// A card in the factory delivery state, UID and signature from the machine seed.
    pub fn new(rng: &mut DetRng) -> Self {
        let mut mem = vec![0u8; MEM_BYTES];
        let mut uid = [0u8; 7];
        rng.stream(RngStream::IDENTITY).fill_bytes(&mut uid);
        // SN0 is the NXP manufacturer byte.
        uid[0] = 0x04;
        mem[0..3].copy_from_slice(&uid[0..3]);
        mem[3] = 0x88 ^ uid[0] ^ uid[1] ^ uid[2];
        mem[4..8].copy_from_slice(&uid[3..7]);
        mem[8] = uid[3] ^ uid[4] ^ uid[5] ^ uid[6];
        // Page 2 byte 1 is UNVERIFIED and stays 0. Page 3: the capability container.
        mem[12..16].copy_from_slice(&[0xE1, 0x10, 0x12, 0x00]);
        // Pages 4 and 5: Lock Control TLV, empty NDEF TLV, terminator.
        mem[16..24].copy_from_slice(&[0x01, 0x03, 0xA0, 0x0C, 0x34, 0x03, 0x00, 0xFE]);
        // Page 0x28: dynamic locks clear, RFUI reads 0xBD.
        mem[PAGE_DYN_LOCK as usize * 4 + 3] = 0xBD;
        // Page 0x29 CFG0: MIRROR with STRG_MOD_EN set, MIRROR_PAGE 0, AUTH0 0xFF (no protection).
        mem[PAGE_CFG0 as usize * 4] = 0x04;
        mem[PAGE_CFG0 as usize * 4 + 3] = 0xFF;
        // CFG1 RFUI bytes are UNVERIFIED and stay 0. PWD is all ones at delivery; PACK stays 0.
        mem[PAGE_PWD as usize * 4..PAGE_PWD as usize * 4 + 4].copy_from_slice(&[0xFF; 4]);
        let mut signature = vec![0u8; 32];
        rng.stream(RngStream::IDENTITY).fill_bytes(&mut signature);
        Ntag213 {
            mem,
            signature,
            counter: 0,
            in_field: false,
            counter_armed: false,
            authenticated: false,
            auth_fails: 0,
            cfg_locked: false,
        }
    }

    /// The 7-byte UID: SN0..SN2 in page 0, SN3..SN6 in page 1.
    pub fn uid(&self) -> [u8; 7] {
        let mut uid = [0u8; 7];
        uid[0..3].copy_from_slice(&self.mem[0..3]);
        uid[3..7].copy_from_slice(&self.mem[4..8]);
        uid
    }

    /// The raw 180 bytes, for `nfc.dump()`.
    pub fn image(&self) -> &[u8] {
        &self.mem
    }

    /// Loads a raw 180-byte image (`nfc.load`); the caller taints the machine.
    pub fn load(&mut self, image: &[u8]) -> Result<(), CardError> {
        if image.len() != MEM_BYTES {
            return Err(CardError::ImageLength(image.len()));
        }
        self.mem.copy_from_slice(image);
        self.field_off();
        // A load swaps the card, so the negative-attempt counter goes with the old one.
        self.auth_fails = 0;
        Ok(())
    }

    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// One page, as stored (without the read masking of PWD and PACK).
    pub fn page(&self, page: u8) -> Option<[u8; PAGE_BYTES]> {
        if page > PAGE_MAX {
            return None;
        }
        let base = page as usize * PAGE_BYTES;
        let mut out = [0u8; PAGE_BYTES];
        out.copy_from_slice(&self.mem[base..base + PAGE_BYTES]);
        Some(out)
    }

    /// AUTH0: the first page password protection covers; 0xFF and above means none.
    pub fn auth0(&self) -> u8 {
        self.mem[PAGE_CFG0 as usize * 4 + 3]
    }

    pub fn access(&self) -> u8 {
        self.mem[PAGE_CFG1 as usize * 4]
    }

    /// PROT: 1 means reads are protected as well as writes above AUTH0.
    pub fn prot(&self) -> bool {
        self.access() & 0x80 != 0
    }

    /// NFC_CNT_EN: whether the counter increments at all.
    pub fn counter_enabled(&self) -> bool {
        self.access() & 0x10 != 0
    }

    /// AUTHLIM: failed PWD_AUTH attempts allowed; 0 means unlimited.
    pub fn authlim(&self) -> u8 {
        self.access() & 0x07
    }

    /// Field on (the tag's power-on reset): clears authentication, arms the counter, latches
    /// CFGLCK.
    pub fn field_on(&mut self) {
        self.in_field = true;
        self.counter_armed = true;
        self.authenticated = false;
        self.cfg_locked = self.access() & 0x40 != 0;
    }

    /// Field off. The AUTHLIM counter survives: per-pass, AUTHLIM 3 would allow unbounded guessing.
    pub fn field_off(&mut self) {
        self.in_field = false;
        self.counter_armed = false;
        self.authenticated = false;
    }

    pub fn in_field(&self) -> bool {
        self.in_field
    }

    pub fn authenticated(&self) -> bool {
        self.authenticated
    }

    /// `nfc.reset()`: back to delivery state, keeping UID and signature (the same physical card).
    pub fn reset_card(&mut self) {
        let uid = self.uid();
        let signature = core::mem::take(&mut self.signature);
        let mut fresh = Ntag213::default();
        fresh.mem[0..3].copy_from_slice(&uid[0..3]);
        fresh.mem[3] = 0x88 ^ uid[0] ^ uid[1] ^ uid[2];
        fresh.mem[4..8].copy_from_slice(&uid[3..7]);
        fresh.mem[8] = uid[3] ^ uid[4] ^ uid[5] ^ uid[6];
        fresh.signature = signature;
        *self = fresh;
    }

    /// Whether a page is locked against writing. LOCK0 bits 3 to 7 and LOCK1 cover pages 3..=15,
    /// one bit each; DLOCK0 and DLOCK1 bits 0 to 3 cover page pairs from 16-17 to 38-39.
    /// Block-locking bits are stored but not consulted.
    pub fn is_locked(&self, page: u8) -> bool {
        let lock0 = self.mem[2 * 4 + 2];
        let lock1 = self.mem[2 * 4 + 3];
        let dlock0 = self.mem[PAGE_DYN_LOCK as usize * 4];
        let dlock1 = self.mem[PAGE_DYN_LOCK as usize * 4 + 1];
        match page {
            3..=7 => lock0 & (1 << page) != 0,
            8..=15 => lock1 & (1 << (page - 8)) != 0,
            16..=31 => dlock0 & (1 << ((page - 16) / 2)) != 0,
            32..=39 => dlock1 & (1 << ((page - 32) / 2)) != 0,
            // CFG0 and CFG1 are locked by CFGLCK, latched at the last field-on.
            PAGE_CFG0 | PAGE_CFG1 => self.cfg_locked,
            _ => false,
        }
    }

    /// Whether a read of `page` is refused: `page >= AUTH0` with PROT set and no authentication.
    fn read_protected(&self, page: u8) -> bool {
        self.prot() && page >= self.auth0() && !self.authenticated
    }

    /// Whether a read returning `pages` is refused. Every page is tested, not just the start, so
    /// `FAST_READ 04 27` cannot leak. Straddling AUTH0 is refused whole (UNVERIFIED).
    fn range_protected(&self, pages: impl IntoIterator<Item = u8>) -> bool {
        pages.into_iter().any(|page| self.read_protected(page))
    }

    /// Whether a write to `page` is refused: `page >= AUTH0` and not authenticated, whatever PROT.
    fn write_protected(&self, page: u8) -> bool {
        page >= self.auth0() && !self.authenticated
    }

    /// One page as a reader sees it: PWD and PACK always read as zeros.
    fn read_page(&self, page: u8) -> [u8; PAGE_BYTES] {
        let mut out = self.page(page).unwrap_or([0; PAGE_BYTES]);
        if page == PAGE_PWD {
            out = [0; PAGE_BYTES];
        } else if page == PAGE_PACK {
            out[0] = 0;
            out[1] = 0;
        }
        out
    }

    /// Increments the counter on the first read of a pass if NFC_CNT_EN is set; saturates.
    fn bump_counter(&mut self) {
        if self.counter_armed && self.counter_enabled() {
            self.counter_armed = false;
            self.counter = self.counter.saturating_add(1).min(COUNTER_MAX);
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CardError {
    ImageLength(usize),
    NoNdefTlv,
    NdefTooLong {
        /// Bytes the message needs, TLV header and terminator included.
        need: usize,
        /// Bytes available from the TLV's start to the end of user memory.
        have: usize,
    },
    MalformedTlv,
}

/// Everything outside the board that the MCU cannot reach: only the card.
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BoardWorld {
    pub card: Ntag213,
}

impl BoardWorld {
    pub fn new(rng: &mut DetRng) -> Self {
        BoardWorld {
            card: Ntag213::new(rng),
        }
    }

    /// Resets one domain. Only `card` reaches the world.
    pub fn reset(&mut self, domain: BoardDomain) {
        if domain == BoardDomain::Card {
            self.card.reset_card();
        }
    }
}

/// Command codes of the ISO/IEC 14443-3 Type A command set.
pub mod cmd {
    pub const GET_VERSION: u8 = 0x60;
    /// READ, four pages with roll-over.
    pub const READ: u8 = 0x30;
    pub const FAST_READ: u8 = 0x3A;
    pub const WRITE: u8 = 0xA2;
    /// COMP_WRITE, the two-phase compatibility write.
    pub const COMP_WRITE: u8 = 0xA0;
    pub const READ_CNT: u8 = 0x39;
    pub const PWD_AUTH: u8 = 0x1B;
    pub const READ_SIG: u8 = 0x3C;
}

impl Ntag213 {
    /// Runs one reader command (without CRC_A). A malformed frame is [`Nak::InvalidArgument`], as
    /// on a real tag.
    pub fn command(&mut self, frame: &[u8]) -> CardResponse {
        let Some((&code, args)) = frame.split_first() else {
            return CardResponse::Nak(Nak::InvalidArgument);
        };
        match (code, args.len()) {
            (cmd::GET_VERSION, 0) => CardResponse::Data(VERSION.to_vec()),
            (cmd::READ, 1) => self.cmd_read(args[0]),
            (cmd::FAST_READ, 2) => self.cmd_fast_read(args[0], args[1]),
            (cmd::WRITE, 5) => self.cmd_write(args[0], &args[1..5]),
            // COMP_WRITE ACKs each phase; of the 16 data bytes only the first 4 are stored.
            (cmd::COMP_WRITE, 1) => {
                if args[0] > PAGE_MAX {
                    CardResponse::Nak(Nak::InvalidArgument)
                } else {
                    CardResponse::Ack
                }
            }
            (cmd::COMP_WRITE, 17) => self.cmd_write(args[0], &args[1..5]),
            (cmd::READ_CNT, 1) => self.cmd_read_cnt(args[0]),
            (cmd::PWD_AUTH, 4) => self.cmd_pwd_auth(&args[0..4]),
            (cmd::READ_SIG, 1) if args[0] == 0 => CardResponse::Data(self.signature.clone()),
            _ => CardResponse::Nak(Nak::InvalidArgument),
        }
    }

    /// READ: four pages with roll-over (READ 0x2A returns 2A, 2B, 2C, 00), all protection-checked.
    fn cmd_read(&mut self, addr: u8) -> CardResponse {
        if addr > PAGE_MAX {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        let pages = [0u8, 1, 2, 3].map(|step| addr.wrapping_add(step) % (PAGE_MAX + 1));
        if self.range_protected(pages) {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        self.bump_counter();
        let mut out = Vec::with_capacity(4 * PAGE_BYTES);
        for page in pages {
            out.extend_from_slice(&self.read_page(page));
        }
        CardResponse::Data(out)
    }

    /// FAST_READ: `(end - start + 1) * 4` bytes, no roll-over, every page protection-checked.
    fn cmd_fast_read(&mut self, start: u8, end: u8) -> CardResponse {
        if start > end || end > PAGE_MAX || self.range_protected(start..=end) {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        self.bump_counter();
        let mut out = Vec::with_capacity((end - start + 1) as usize * PAGE_BYTES);
        for page in start..=end {
            out.extend_from_slice(&self.read_page(page));
        }
        CardResponse::Data(out)
    }

    /// WRITE one page. Pages 2, 3 and 0x28 are OTP (OR-ed in; page 2 ignores its first two bytes).
    /// The UID pages 0 and 1 are read-only.
    fn cmd_write(&mut self, page: u8, data: &[u8]) -> CardResponse {
        if !(2..=PAGE_MAX).contains(&page) {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        if self.write_protected(page) {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        if self.is_locked(page) {
            return CardResponse::Nak(Nak::WriteError);
        }
        let base = page as usize * PAGE_BYTES;
        match page {
            2 => {
                self.mem[base + 2] |= data[2];
                self.mem[base + 3] |= data[3];
            }
            3 | PAGE_DYN_LOCK => {
                for (stored, new) in self.mem[base..base + PAGE_BYTES].iter_mut().zip(data) {
                    *stored |= *new;
                }
            }
            _ => self.mem[base..base + PAGE_BYTES].copy_from_slice(data),
        }
        CardResponse::Ack
    }

    /// READ_CNT (argument 0x02): the counter as 3 bytes, least significant first (UNVERIFIED).
    fn cmd_read_cnt(&mut self, addr: u8) -> CardResponse {
        if addr != 0x02 {
            return CardResponse::Nak(Nak::InvalidArgument);
        }
        let counter = self.counter;
        CardResponse::Data(vec![
            counter as u8,
            (counter >> 8) as u8,
            (counter >> 16) as u8,
        ])
    }

    /// PWD_AUTH: PACK on a match, a NAK counted against AUTHLIM otherwise. At the limit every
    /// PWD_AUTH gets [`Nak::AuthOverflow`] until `nfc.reset()`.
    fn cmd_pwd_auth(&mut self, pwd: &[u8]) -> CardResponse {
        let limit = self.authlim();
        if limit != 0 && self.auth_fails >= limit {
            return CardResponse::Nak(Nak::AuthOverflow);
        }
        let base = PAGE_PWD as usize * PAGE_BYTES;
        if self.mem[base..base + 4] == *pwd {
            self.authenticated = true;
            self.auth_fails = 0;
            let pack = PAGE_PACK as usize * PAGE_BYTES;
            return CardResponse::Data(self.mem[pack..pack + 2].to_vec());
        }
        self.auth_fails = self.auth_fails.saturating_add(1);
        if limit != 0 && self.auth_fails >= limit {
            CardResponse::Nak(Nak::AuthOverflow)
        } else {
            CardResponse::Nak(Nak::InvalidArgument)
        }
    }
}

/// NDEF on a Type 2 Tag: TLV framing and record encodings over byte slices.
pub mod ndef {
    use super::CardError;

    /// Type byte of the NULL TLV, one byte of padding.
    pub const TLV_NULL: u8 = 0x00;
    /// Type byte of the Lock Control TLV, which the delivery state puts before the message.
    pub const TLV_LOCK_CONTROL: u8 = 0x01;
    pub const TLV_MEMORY_CONTROL: u8 = 0x02;
    pub const TLV_NDEF: u8 = 0x03;
    pub const TLV_TERMINATOR: u8 = 0xFE;

    /// URI prefixes by identifier code. The URI RTD runs to 0x23, but only these are confirmed;
    /// unknown codes are never emitted and are left in place when decoded.
    pub const URI_PREFIXES: [&str; 7] = [
        "",
        "http://www.",
        "https://www.",
        "http://",
        "https://",
        "tel:",
        "mailto:",
    ];

    #[derive(Clone, PartialEq, Eq, Debug)]
    pub enum NdefRecord {
        /// A well-known URI record (type `U`), stored with the shortest matching prefix code.
        Uri(String),
        /// A well-known Text record (type `T`), UTF-8 with a language code.
        Text { lang: String, text: String },
        /// A MIME media record (TNF 2), such as Wi-Fi Simple Configuration.
        Mime { mime_type: String, payload: Vec<u8> },
        /// Anything else, kept byte-exact so decode then encode round-trips.
        Other {
            /// Type Name Format, the low 3 bits of the header byte.
            tnf: u8,
            type_bytes: Vec<u8>,
            payload: Vec<u8>,
        },
    }

    /// The prefix code and remainder for a URI: the longest [`URI_PREFIXES`] entry it starts with.
    pub fn encode_uri(uri: &str) -> (u8, &str) {
        let mut best = (0u8, uri);
        for (code, prefix) in URI_PREFIXES.iter().enumerate() {
            if !prefix.is_empty()
                && uri.starts_with(prefix)
                && prefix.len() > URI_PREFIXES[best.0 as usize].len()
            {
                best = (code as u8, &uri[prefix.len()..]);
            }
        }
        best
    }

    /// Expands a URI record payload back into a URI; an unknown prefix code is left as is.
    pub fn decode_uri(code: u8, rest: &str) -> String {
        match URI_PREFIXES.get(code as usize) {
            Some(prefix) => format!("{prefix}{rest}"),
            None => rest.to_string(),
        }
    }

    fn record_parts(record: &NdefRecord) -> (u8, Vec<u8>, Vec<u8>) {
        match record {
            NdefRecord::Uri(uri) => {
                let (code, rest) = encode_uri(uri);
                let mut payload = vec![code];
                payload.extend_from_slice(rest.as_bytes());
                (1, b"U".to_vec(), payload)
            }
            NdefRecord::Text { lang, text } => {
                // Status byte: bit 7 clear for UTF-8, low 6 bits the language code length.
                let mut payload = vec![(lang.len() & 0x3F) as u8];
                payload.extend_from_slice(lang.as_bytes());
                payload.extend_from_slice(text.as_bytes());
                (1, b"T".to_vec(), payload)
            }
            NdefRecord::Mime { mime_type, payload } => {
                (2, mime_type.as_bytes().to_vec(), payload.clone())
            }
            NdefRecord::Other {
                tnf,
                type_bytes,
                payload,
            } => (*tnf, type_bytes.clone(), payload.clone()),
        }
    }

    /// Encodes a message: MB first, ME last, SR with a 1-byte length under 256 bytes.
    pub fn encode_message(records: &[NdefRecord]) -> Vec<u8> {
        let mut out = Vec::new();
        for (index, record) in records.iter().enumerate() {
            let (tnf, type_bytes, payload) = record_parts(record);
            let short = payload.len() < 256;
            let mut header = tnf & 0x07;
            if index == 0 {
                header |= 0x80;
            }
            if index + 1 == records.len() {
                header |= 0x40;
            }
            if short {
                header |= 0x10;
            }
            out.push(header);
            out.push(type_bytes.len() as u8);
            if short {
                out.push(payload.len() as u8);
            } else {
                out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            }
            out.extend_from_slice(&type_bytes);
            out.extend_from_slice(&payload);
        }
        out
    }

    /// Decodes a message; anything but `U`, `T` or a MIME type becomes [`NdefRecord::Other`].
    pub fn decode_message(bytes: &[u8]) -> Result<Vec<NdefRecord>, CardError> {
        let mut records = Vec::new();
        let mut at = 0usize;
        while at < bytes.len() {
            let header = bytes[at];
            at += 1;
            let short = header & 0x10 != 0;
            let has_id = header & 0x08 != 0;
            let tnf = header & 0x07;
            let type_len = *bytes.get(at).ok_or(CardError::MalformedTlv)? as usize;
            at += 1;
            let payload_len = if short {
                let len = *bytes.get(at).ok_or(CardError::MalformedTlv)? as usize;
                at += 1;
                len
            } else {
                let raw: [u8; 4] = bytes
                    .get(at..at + 4)
                    .ok_or(CardError::MalformedTlv)?
                    .try_into()
                    .map_err(|_| CardError::MalformedTlv)?;
                at += 4;
                u32::from_be_bytes(raw) as usize
            };
            let id_len = if has_id {
                let len = *bytes.get(at).ok_or(CardError::MalformedTlv)? as usize;
                at += 1;
                len
            } else {
                0
            };
            let type_bytes = bytes
                .get(at..at + type_len)
                .ok_or(CardError::MalformedTlv)?
                .to_vec();
            at += type_len + id_len;
            let payload = bytes
                .get(at..at + payload_len)
                .ok_or(CardError::MalformedTlv)?
                .to_vec();
            at += payload_len;
            records.push(classify(tnf, type_bytes, payload));
            if header & 0x40 != 0 {
                break;
            }
        }
        Ok(records)
    }

    fn classify(tnf: u8, type_bytes: Vec<u8>, payload: Vec<u8>) -> NdefRecord {
        if tnf == 1
            && type_bytes == b"U"
            && !payload.is_empty()
            && let Ok(rest) = core::str::from_utf8(&payload[1..])
        {
            return NdefRecord::Uri(decode_uri(payload[0], rest));
        }
        if tnf == 1 && type_bytes == b"T" && !payload.is_empty() {
            let lang_len = (payload[0] & 0x3F) as usize;
            if payload[0] & 0x80 == 0 && payload.len() > lang_len {
                let lang = core::str::from_utf8(&payload[1..1 + lang_len]);
                let text = core::str::from_utf8(&payload[1 + lang_len..]);
                if let (Ok(lang), Ok(text)) = (lang, text) {
                    return NdefRecord::Text {
                        lang: lang.to_string(),
                        text: text.to_string(),
                    };
                }
            }
        }
        if tnf == 2
            && let Ok(mime_type) = core::str::from_utf8(&type_bytes)
        {
            return NdefRecord::Mime {
                mime_type: mime_type.to_string(),
                payload,
            };
        }
        NdefRecord::Other {
            tnf,
            type_bytes,
            payload,
        }
    }

    #[derive(Copy, Clone, PartialEq, Eq, Debug)]
    pub struct NdefTlv {
        /// Offset of the `03` type byte in the area.
        pub start: usize,
        /// Offset of the value.
        pub value: usize,
        pub len: usize,
    }

    /// Finds the NDEF Message TLV, skipping NULL, Lock Control and Memory Control TLVs.
    pub fn find_ndef(area: &[u8]) -> Result<NdefTlv, CardError> {
        let mut at = 0usize;
        while at < area.len() {
            let tag = area[at];
            if tag == TLV_TERMINATOR {
                return Err(CardError::NoNdefTlv);
            }
            if tag == TLV_NULL {
                at += 1;
                continue;
            }
            let (len, header) = tlv_length(area, at + 1)?;
            if tag == TLV_NDEF {
                return Ok(NdefTlv {
                    start: at,
                    value: at + 1 + header,
                    len,
                });
            }
            at = at + 1 + header + len;
        }
        Err(CardError::NoNdefTlv)
    }

    /// A TLV length field and its size (255 or more is `FF hh ll`).
    fn tlv_length(area: &[u8], at: usize) -> Result<(usize, usize), CardError> {
        match area.get(at) {
            None => Err(CardError::MalformedTlv),
            Some(&0xFF) => {
                let hi = *area.get(at + 1).ok_or(CardError::MalformedTlv)? as usize;
                let lo = *area.get(at + 2).ok_or(CardError::MalformedTlv)? as usize;
                Ok(((hi << 8) | lo, 3))
            }
            Some(&len) => Ok((len as usize, 1)),
        }
    }

    pub fn tlv_header(len: usize) -> Vec<u8> {
        if len < 0xFF {
            vec![TLV_NDEF, len as u8]
        } else {
            vec![TLV_NDEF, 0xFF, (len >> 8) as u8, len as u8]
        }
    }
}

impl Ntag213 {
    /// The user memory as one TLV area: pages 0x04 to 0x27, 144 bytes.
    fn user_area(&self) -> &[u8] {
        let start = USER_FIRST as usize * PAGE_BYTES;
        let end = (USER_LAST as usize + 1) * PAGE_BYTES;
        &self.mem[start..end]
    }

    /// `nfc.ndef.read()`: the records of the NDEF Message TLV in user memory.
    pub fn ndef_read(&self) -> Result<Vec<ndef::NdefRecord>, CardError> {
        let area = self.user_area();
        let tlv = ndef::find_ndef(area)?;
        let bytes = area
            .get(tlv.value..tlv.value + tlv.len)
            .ok_or(CardError::MalformedTlv)?;
        ndef::decode_message(bytes)
    }

    /// `nfc.ndef.write(records)`: replaces the NDEF Message TLV, keeping the TLVs before it. Page
    /// locks do not apply to the host's tap; send WRITE commands for that.
    pub fn ndef_write(&mut self, records: &[ndef::NdefRecord]) -> Result<(), CardError> {
        let message = ndef::encode_message(records);
        let header = ndef::tlv_header(message.len());
        let start = USER_FIRST as usize * PAGE_BYTES;
        let end = (USER_LAST as usize + 1) * PAGE_BYTES;
        let tlv = ndef::find_ndef(&self.mem[start..end])?;
        let need = header.len() + message.len() + 1;
        let have = (end - start) - tlv.start;
        if need > have {
            return Err(CardError::NdefTooLong { need, have });
        }
        let at = start + tlv.start;
        self.mem[at..at + header.len()].copy_from_slice(&header);
        let body = at + header.len();
        self.mem[body..body + message.len()].copy_from_slice(&message);
        self.mem[body + message.len()] = ndef::TLV_TERMINATOR;
        for byte in self.mem[body + message.len() + 1..end].iter_mut() {
            *byte = 0;
        }
        Ok(())
    }
}
