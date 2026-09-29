//! The secret set builder shared by `xtask secrets-check` and the runtime redaction pass. Pure over
//! bytes (no file system, environment, time or randomness), so it builds for wasm32. Callers read
//! the device files and pick the salt.
//!
//! Members:
//! - the base MAC (eFuse BLK1 bits 0-47) and base+1 to base+3, as raw bytes and colon, dash and
//!   bare hex in both cases and both byte orders. ESP-IDF `mac_addr.c` adds to the last byte only
//!   (wrapping); the 48-bit carry form is added too;
//! - the 3-byte NIC suffix of each of those MACs, forward text forms only;
//! - the unique ID (BLK2 bits 0-127), raw and hex, both byte orders;
//! - each non-zero BLK2 calibration word (words 4 to 7), raw and hex, both byte orders;
//! - device backup file stems;
//! - the non-0xFF cardid window content, one member per 32-byte chunk, except valid NVS page
//!   headers at page boundaries, which every NVS partition holds;
//! - at run time: NVS credential values of 6 or more bytes (raw, hex, base64), the NFC UID, PWD and
//!   PACK, and the guard canary.
//!
//! False-positive guards (tuned so a hashed check does not refuse every binary): members shorter
//! than [`MIN_MEMBER_LEN`], uniform members, calibration words with fewer than 2 non-zero bytes and
//! backup stems shorter than [`MIN_CREDENTIAL_LEN`] are dropped and counted.
//!
//! [`SaltedSet`] holds `sha256(salt || member)` per member and scans every window of every member
//! length. Text-only members match only in text-like content (git's rule: no NUL in the first
//! [`TEXT_PROBE_LEN`] bytes). Nothing here renders a member, a matched window or a hash in `Debug`
//! or `Display`; hits carry kind, offset and length only.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use sha2::{Digest, Sha256};

/// Byte length of eFuse BLK0 and BLK1 dumps (espefuse layout).
pub const EFUSE_SHORT_BLOCK_LEN: usize = 24;
/// Byte length of eFuse BLK2 to BLK10 dumps.
pub const EFUSE_LONG_BLOCK_LEN: usize = 32;
pub const CARDID_CHUNK_LEN: usize = 32;
/// One flash sector. A cardid chunk at a multiple of it that is a valid NVS page header is not a
/// member.
pub const NVS_PAGE_LEN: usize = 0x1000;
const NVS_PAGE_HEADER_LEN: usize = 32;
/// ACTIVE, FULL, FREEING, CORRUPT (ESP-IDF `nvs_constants.h`).
const NVS_PAGE_STATES: [u32; 4] = [0xFFFF_FFFE, 0xFFFF_FFFC, 0xFFFF_FFF8, 0xFFFF_FFF0];
pub const MIN_MEMBER_LEN: usize = 4;
pub const MIN_CREDENTIAL_LEN: usize = 6;
pub const MIN_SALT_LEN: usize = 16;
/// The same window git uses.
pub const TEXT_PROBE_LEN: usize = 8000;

/// The name is stable and is what `secrets-check.toml` stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MemberKind {
    Mac,
    MacSuffix,
    UniqueId,
    CalibWord,
    BackupStem,
    CardId,
    NvsCredential,
    NfcUid,
    NfcPwd,
    NfcPack,
    Canary,
}

impl MemberKind {
    pub const ALL: [MemberKind; 11] = [
        MemberKind::Mac,
        MemberKind::MacSuffix,
        MemberKind::UniqueId,
        MemberKind::CalibWord,
        MemberKind::BackupStem,
        MemberKind::CardId,
        MemberKind::NvsCredential,
        MemberKind::NfcUid,
        MemberKind::NfcPwd,
        MemberKind::NfcPack,
        MemberKind::Canary,
    ];

    pub fn name(self) -> &'static str {
        match self {
            MemberKind::Mac => "mac",
            MemberKind::MacSuffix => "mac_suffix",
            MemberKind::UniqueId => "unique_id",
            MemberKind::CalibWord => "calib_word",
            MemberKind::BackupStem => "backup_stem",
            MemberKind::CardId => "cardid",
            MemberKind::NvsCredential => "nvs_credential",
            MemberKind::NfcUid => "nfc_uid",
            MemberKind::NfcPwd => "nfc_pwd",
            MemberKind::NfcPack => "nfc_pack",
            MemberKind::Canary => "canary",
        }
    }

    pub fn from_name(name: &str) -> Option<MemberKind> {
        MemberKind::ALL.into_iter().find(|k| k.name() == name)
    }

    /// Whether a match is replaced by `<MAC>` rather than `<SECRET>`.
    pub fn is_mac(self) -> bool {
        matches!(self, MemberKind::Mac | MemberKind::MacSuffix)
    }
}

impl fmt::Display for MemberKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct Member {
    pub kind: MemberKind,
    pub bytes: Vec<u8>,
    pub text_only: bool,
}

impl fmt::Debug for Member {
    // Never print the bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Member")
            .field("kind", &self.kind)
            .field("len", &self.bytes.len())
            .field("text_only", &self.text_only)
            .finish()
    }
}

/// Builder and salted-set errors. They name positions and lengths, never content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SecretSetError {
    BlockIndex(u8),
    BlockLength {
        index: u8,
        expected: usize,
        actual: usize,
    },
    SaltTooShort(usize),
}

impl fmt::Display for SecretSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretSetError::BlockIndex(i) => write!(f, "eFuse block index {i} is outside 0..=10"),
            SecretSetError::BlockLength {
                index,
                expected,
                actual,
            } => write!(
                f,
                "eFuse block {index} has {actual} bytes, expected {expected}"
            ),
            SecretSetError::SaltTooShort(n) => {
                write!(f, "salt has {n} bytes, at least {MIN_SALT_LEN} required")
            }
        }
    }
}

impl std::error::Error for SecretSetError {}

const HEX_LOWER: &[u8; 16] = b"0123456789abcdef";
const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode_hex(bytes: &[u8], upper: bool, sep: Option<u8>) -> Vec<u8> {
    let digits = if upper { HEX_UPPER } else { HEX_LOWER };
    let mut out = Vec::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if let (true, Some(s)) = (i > 0, sep) {
            out.push(s);
        }
        out.push(digits[usize::from(b >> 4)]);
        out.push(digits[usize::from(b & 0x0f)]);
    }
    out
}

pub fn hex_string(bytes: &[u8]) -> String {
    encode_hex(bytes, false, None)
        .into_iter()
        .map(char::from)
        .collect()
}

/// `None` on odd length or a non-hex character.
pub fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let t = text.as_bytes();
    if !t.len().is_multiple_of(2) {
        return None;
    }
    t.chunks_exact(2)
        .map(|p| Some((hex_digit(p[0])? << 4) | hex_digit(p[1])?))
        .collect()
}

fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Standard base64 of `bytes`, keeping only the characters `bytes` fully determine: no padding and
/// no final character mixing in bits of a following byte. The result is a prefix of the encoding of
/// any longer buffer that starts with `bytes`.
pub fn encode_base64_prefix(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        let full = if chunk.len() == 3 { 4 } else { chunk.len() };
        for i in 0..full {
            out.push(BASE64[((n >> (18 - 6 * i)) & 0x3f) as usize]);
        }
    }
    out
}

fn separated_hex_forms(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(6);
    for upper in [false, true] {
        for sep in [Some(b':'), Some(b'-'), None] {
            out.push(encode_hex(bytes, upper, sep));
        }
    }
    out
}

fn is_uniform(bytes: &[u8]) -> bool {
    bytes.windows(2).all(|w| w[0] == w[1])
}

/// CRC32 as ESP-IDF NVS computes it: reflected polynomial 0xEDB88320, register init 0, final xor
/// 0xFFFFFFFF. Unlike the common CRC-32, the register does not start at all ones.
fn nvs_crc32(bytes: &[u8]) -> u32 {
    let mut reg = 0u32;
    for &byte in bytes {
        reg ^= u32::from(byte);
        for _ in 0..8 {
            reg = if reg & 1 != 0 {
                (reg >> 1) ^ 0xEDB8_8320
            } else {
                reg >> 1
            };
        }
    }
    !reg
}

/// 32 bytes: a known state word (0), sequence number (4), version 0xFE or 0xFF (8), 19 reserved
/// 0xFF bytes (9 to 27) and the CRC32 of bytes 4 to 27 (28).
fn is_nvs_page_header(chunk: &[u8]) -> bool {
    let word =
        |at: usize| u32::from_le_bytes([chunk[at], chunk[at + 1], chunk[at + 2], chunk[at + 3]]);
    chunk.len() == NVS_PAGE_HEADER_LEN
        && NVS_PAGE_STATES.contains(&word(0))
        && matches!(chunk[8], 0xFE | 0xFF)
        && chunk[9..28].iter().all(|&b| b == 0xFF)
        && word(28) == nvs_crc32(&chunk[4..28])
}

fn mac_to_u64(mac: [u8; 6]) -> u64 {
    mac.iter().fold(0, |acc, &b| (acc << 8) | u64::from(b))
}

fn u64_to_mac(v: u64) -> [u8; 6] {
    let b = v.to_be_bytes();
    [b[2], b[3], b[4], b[5], b[6], b[7]]
}

/// Methods take `&mut self` so a machine can keep adding runtime members.
#[derive(Clone, Default)]
pub struct SecretSetBuilder {
    /// A byte string is one member. On a repeat the first kind stays, and `text_only` becomes false
    /// if any contributor allowed binary matches.
    members: BTreeMap<Vec<u8>, (MemberKind, bool)>,
    dropped: usize,
    nvs_headers_skipped: usize,
}

impl fmt::Debug for SecretSetBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSetBuilder")
            .field("members", &self.members.len())
            .field("dropped", &self.dropped)
            .field("nvs_headers_skipped", &self.nvs_headers_skipped)
            .finish()
    }
}

impl SecretSetBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_set(set: SecretSet) -> Self {
        let members = set
            .members
            .into_iter()
            .map(|m| (m.bytes, (m.kind, m.text_only)));
        SecretSetBuilder {
            members: members.collect(),
            dropped: set.dropped,
            nvs_headers_skipped: set.nvs_headers_skipped,
        }
    }

    /// BLK1 yields the MAC members, BLK2 the unique ID and calibration words; other blocks are only
    /// length-checked.
    pub fn efuse_block(&mut self, index: u8, bytes: &[u8]) -> Result<&mut Self, SecretSetError> {
        let expected = match index {
            0 | 1 => EFUSE_SHORT_BLOCK_LEN,
            2..=10 => EFUSE_LONG_BLOCK_LEN,
            _ => return Err(SecretSetError::BlockIndex(index)),
        };
        if bytes.len() != expected {
            return Err(SecretSetError::BlockLength {
                index,
                expected,
                actual: bytes.len(),
            });
        }
        match index {
            1 => {
                // Most significant byte at bits 40-47.
                let mut mac = [0u8; 6];
                for (i, m) in mac.iter_mut().enumerate() {
                    *m = bytes[5 - i];
                }
                self.base_mac(mac);
            }
            2 => {
                let mut uid = [0u8; 16];
                uid.copy_from_slice(&bytes[..16]);
                self.unique_id(uid);
                for w in bytes[16..32].chunks_exact(4) {
                    self.calibration_word(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                }
            }
            _ => {}
        }
        Ok(self)
    }

    /// Base+1 to base+3 are Wi-Fi station, soft-AP, BT and Ethernet, in last-byte wrap form and
    /// 48-bit carry form. A uniform MAC (unprogrammed eFuse) adds nothing.
    pub fn base_mac(&mut self, mac: [u8; 6]) -> &mut Self {
        if is_uniform(&mac) {
            self.dropped += 1;
            return self;
        }
        let base = mac_to_u64(mac);
        for offset in 0..=3u8 {
            let mut wrap = mac;
            wrap[5] = wrap[5].wrapping_add(offset);
            let carry = u64_to_mac((base + u64::from(offset)) & 0xFFFF_FFFF_FFFF);
            self.one_mac(wrap);
            if carry != wrap {
                self.one_mac(carry);
            }
        }
        self
    }

    fn one_mac(&mut self, mac: [u8; 6]) {
        let mut reversed = mac;
        reversed.reverse();
        for order in [mac, reversed] {
            self.push(MemberKind::Mac, &order, false);
            for text in separated_hex_forms(&order) {
                self.push(MemberKind::Mac, &text, false);
            }
        }
        // The raw 3-byte suffix is never a member: it would match binaries by chance.
        if is_uniform(&mac[3..]) {
            self.dropped += 1;
            return;
        }
        for text in separated_hex_forms(&mac[3..]) {
            self.push(MemberKind::MacSuffix, &text, true);
        }
    }

    /// A uniform ID (unprogrammed) adds nothing.
    pub fn unique_id(&mut self, uid: [u8; 16]) -> &mut Self {
        if is_uniform(&uid) {
            self.dropped += 1;
            return self;
        }
        let mut reversed = uid;
        reversed.reverse();
        for order in [uid, reversed] {
            self.push(MemberKind::UniqueId, &order, false);
            for upper in [false, true] {
                self.push(
                    MemberKind::UniqueId,
                    &encode_hex(&order, upper, None),
                    false,
                );
            }
        }
        self
    }

    /// A zero word adds nothing; one with fewer than 2 non-zero bytes is dropped.
    pub fn calibration_word(&mut self, word: u32) -> &mut Self {
        if word == 0 {
            return self;
        }
        let le = word.to_le_bytes();
        if le.iter().filter(|&&b| b != 0).count() < 2 {
            self.dropped += 1;
            return self;
        }
        for order in [le, word.to_be_bytes()] {
            self.push(MemberKind::CalibWord, &order, false);
            for upper in [false, true] {
                self.push(
                    MemberKind::CalibWord,
                    &encode_hex(&order, upper, None),
                    false,
                );
            }
        }
        self
    }

    /// Shorter stems are ordinary words, not identity.
    pub fn backup_stem(&mut self, stem: &str) -> &mut Self {
        if stem.len() < MIN_CREDENTIAL_LEN {
            self.dropped += 1;
            return self;
        }
        self.push(MemberKind::BackupStem, stem.as_bytes(), false);
        self
    }

    /// One member per 32-byte chunk from the window start; all-0xFF chunks are skipped. A chunk at
    /// a multiple of [`NVS_PAGE_LEN`] that is a valid NVS page header is skipped and counted: it
    /// matches the headers of any NVS partition. The window is expected to start on a page
    /// boundary.
    pub fn cardid_window(&mut self, window: &[u8]) -> &mut Self {
        for (index, chunk) in window.chunks(CARDID_CHUNK_LEN).enumerate() {
            if chunk.iter().all(|&b| b == 0xFF) {
                continue;
            }
            let offset = index * CARDID_CHUNK_LEN;
            if offset.is_multiple_of(NVS_PAGE_LEN) && is_nvs_page_header(chunk) {
                self.nvs_headers_skipped += 1;
                continue;
            }
            self.push(MemberKind::CardId, chunk, false);
        }
        self
    }

    /// One trailing NUL is ignored.
    pub fn nvs_credential(&mut self, value: &[u8]) -> &mut Self {
        let v = value.strip_suffix(&[0]).unwrap_or(value);
        if v.len() < MIN_CREDENTIAL_LEN {
            return self;
        }
        if is_uniform(v) {
            self.dropped += 1;
            return self;
        }
        self.push(MemberKind::NvsCredential, v, false);
        for upper in [false, true] {
            self.push(
                MemberKind::NvsCredential,
                &encode_hex(v, upper, None),
                false,
            );
        }
        self.push(MemberKind::NvsCredential, &encode_base64_prefix(v), false);
        self
    }

    pub fn nfc_uid(&mut self, uid: &[u8]) -> &mut Self {
        self.value_forms(MemberKind::NfcUid, uid)
    }

    pub fn nfc_pwd(&mut self, pwd: &[u8]) -> &mut Self {
        self.value_forms(MemberKind::NfcPwd, pwd)
    }

    /// Its 2 raw bytes are below [`MIN_MEMBER_LEN`], so only the text forms remain, text-only.
    pub fn nfc_pack(&mut self, pack: &[u8]) -> &mut Self {
        self.value_forms(MemberKind::NfcPack, pack)
    }

    pub fn canary(&mut self, canary: &[u8]) -> &mut Self {
        self.value_forms(MemberKind::Canary, canary)
    }

    /// Text forms of values shorter than 3 bytes are text-only.
    fn value_forms(&mut self, kind: MemberKind, v: &[u8]) -> &mut Self {
        if is_uniform(v) {
            self.dropped += 1;
            return self;
        }
        self.push(kind, v, false);
        let short = v.len() < 3;
        for text in separated_hex_forms(v) {
            self.push(kind, &text, short);
        }
        self
    }

    fn push(&mut self, kind: MemberKind, bytes: &[u8], text_only: bool) {
        if bytes.len() < MIN_MEMBER_LEN || is_uniform(bytes) {
            self.dropped += 1;
            return;
        }
        self.members
            .entry(bytes.to_vec())
            .and_modify(|e| e.1 &= text_only)
            .or_insert((kind, text_only));
    }

    /// Members ordered by bytes, so the result is deterministic.
    pub fn build(self) -> SecretSet {
        let members = self
            .members
            .into_iter()
            .map(|(bytes, (kind, text_only))| Member {
                kind,
                bytes,
                text_only,
            })
            .collect();
        SecretSet {
            members,
            dropped: self.dropped,
            nvs_headers_skipped: self.nvs_headers_skipped,
        }
    }
}

/// The identity members of one device. `Debug` prints counts only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct SecretSet {
    members: Vec<Member>,
    dropped: usize,
    nvs_headers_skipped: usize,
}

impl fmt::Debug for SecretSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretSet")
            .field("members", &self.members.len())
            .field("dropped", &self.dropped)
            .field("nvs_headers_skipped", &self.nvs_headers_skipped)
            .finish()
    }
}

impl SecretSet {
    pub fn builder() -> SecretSetBuilder {
        SecretSetBuilder::new()
    }

    pub fn members(&self) -> &[Member] {
        &self.members
    }

    pub fn len(&self) -> usize {
        self.members.len()
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    pub fn dropped(&self) -> usize {
        self.dropped
    }

    pub fn nvs_headers_skipped(&self) -> usize {
        self.nvs_headers_skipped
    }

    pub fn count_by_kind(&self) -> BTreeMap<MemberKind, usize> {
        count_kinds(self.members.iter().map(|m| m.kind))
    }

    pub fn salted(&self, salt: &[u8]) -> Result<SaltedSet, SecretSetError> {
        SaltedSet::new(self, salt)
    }
}

fn count_kinds(kinds: impl Iterator<Item = MemberKind>) -> BTreeMap<MemberKind, usize> {
    let mut out = BTreeMap::new();
    for k in kinds {
        *out.entry(k).or_insert(0) += 1;
    }
    out
}

pub fn salted_hash(salt: &[u8], bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::new()
        .chain_update(salt)
        .chain_update(bytes)
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// `Debug` omits the hash.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SaltedEntry {
    pub hash: [u8; 32],
    pub kind: MemberKind,
    /// Also the window length that can match it.
    pub len: usize,
    pub text_only: bool,
}

impl fmt::Debug for SaltedEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaltedEntry")
            .field("kind", &self.kind)
            .field("len", &self.len)
            .field("text_only", &self.text_only)
            .finish()
    }
}

/// What `secrets-check.toml` stores.
#[derive(Clone, PartialEq, Eq)]
pub struct SaltedSet {
    salt: Vec<u8>,
    /// Sorted by (hash, len), unique.
    entries: Vec<SaltedEntry>,
    lens: BTreeSet<usize>,
    /// Lengths that have at least one entry allowed to match binary content.
    binary_lens: BTreeSet<usize>,
}

impl fmt::Debug for SaltedSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SaltedSet")
            .field("salt_len", &self.salt.len())
            .field("entries", &self.entries.len())
            .field("window_lens", &self.lens)
            .finish()
    }
}

/// Never the matched bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hit {
    pub offset: usize,
    pub len: usize,
    pub kind: MemberKind,
}

/// Git's binary detection rule; UTF-8 validity is not required.
pub fn is_text_like(data: &[u8]) -> bool {
    !data[..data.len().min(TEXT_PROBE_LEN)].contains(&0)
}

impl SaltedSet {
    /// The caller generates the salt.
    pub fn new(set: &SecretSet, salt: &[u8]) -> Result<SaltedSet, SecretSetError> {
        let entries = set
            .members
            .iter()
            .map(|m| SaltedEntry {
                hash: salted_hash(salt, &m.bytes),
                kind: m.kind,
                len: m.bytes.len(),
                text_only: m.text_only,
            })
            .collect();
        SaltedSet::from_parts(salt.to_vec(), entries)
    }

    /// The `secrets-check.toml` loader. Zero-length entries are ignored.
    pub fn from_parts(
        salt: Vec<u8>,
        mut entries: Vec<SaltedEntry>,
    ) -> Result<SaltedSet, SecretSetError> {
        if salt.len() < MIN_SALT_LEN {
            return Err(SecretSetError::SaltTooShort(salt.len()));
        }
        entries.retain(|e| e.len > 0);
        entries.sort_by(|a, b| (a.hash, a.len).cmp(&(b.hash, b.len)));
        entries.dedup_by(|later, kept| {
            let same = later.hash == kept.hash && later.len == kept.len;
            if same {
                kept.text_only &= later.text_only;
            }
            same
        });
        let lens = entries.iter().map(|e| e.len).collect();
        let binary_lens = entries
            .iter()
            .filter(|e| !e.text_only)
            .map(|e| e.len)
            .collect();
        Ok(SaltedSet {
            salt,
            entries,
            lens,
            binary_lens,
        })
    }

    pub fn salt(&self) -> &[u8] {
        &self.salt
    }

    pub fn entries(&self) -> &[SaltedEntry] {
        &self.entries
    }

    /// Every window length the scanner checks.
    pub fn window_lens(&self) -> &BTreeSet<usize> {
        &self.lens
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn count_by_kind(&self) -> BTreeMap<MemberKind, usize> {
        count_kinds(self.entries.iter().map(|e| e.kind))
    }

    fn lookup(&self, hash: &[u8; 32], len: usize) -> Option<&SaltedEntry> {
        self.entries
            .binary_search_by(|e| (&e.hash, e.len).cmp(&(hash, len)))
            .ok()
            .map(|i| &self.entries[i])
    }

    pub fn scan(&self, data: &[u8]) -> Vec<Hit> {
        self.scan_as(data, is_text_like(data))
    }

    /// Hits are ordered by offset, then length. Uniform windows are skipped, since no member is
    /// uniform.
    pub fn scan_as(&self, data: &[u8], text_like: bool) -> Vec<Hit> {
        let lens: Vec<usize> = if text_like {
            &self.lens
        } else {
            &self.binary_lens
        }
        .iter()
        .copied()
        .collect();
        let mut hits = Vec::new();
        if lens.is_empty() {
            return hits;
        }
        let salted = Sha256::new().chain_update(&self.salt);
        // End of the run of bytes equal to data[i], starting at i.
        let mut run_end = 0;
        for i in 0..data.len() {
            if i == 0 || data[i] != data[i - 1] {
                run_end = i + 1;
                while run_end < data.len() && data[run_end] == data[i] {
                    run_end += 1;
                }
            }
            for &len in &lens {
                if i + len > data.len() {
                    break;
                }
                if run_end - i >= len {
                    continue;
                }
                let digest = salted.clone().chain_update(&data[i..i + len]).finalize();
                let mut hash = [0u8; 32];
                hash.copy_from_slice(&digest);
                if let Some(e) = self.lookup(&hash, len)
                    && (text_like || !e.text_only)
                {
                    hits.push(Hit {
                        offset: i,
                        len,
                        kind: e.kind,
                    });
                }
            }
        }
        hits
    }
}

/// Counts only, never member values.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelfTestReport {
    pub members: usize,
    pub detected: usize,
    pub missed: BTreeMap<MemberKind, usize>,
}

impl SelfTestReport {
    pub fn passed(&self) -> bool {
        self.members > 0 && self.detected == self.members
    }
}

/// The `secrets-check --self-test`: renders every member in memory and checks `salted` finds it
/// there. Text-only members go in a text line and the others between binary bytes, so both paths of
/// [`SaltedSet::scan`] run.
pub fn self_test(set: &SecretSet, salted: &SaltedSet) -> SelfTestReport {
    let mut report = SelfTestReport::default();
    for m in &set.members {
        let (prefix, suffix): (&[u8], &[u8]) = if m.text_only {
            (b"member: ", b" end\n")
        } else {
            (&[0x00, 0x01, 0x02, 0x03], &[0x00, 0xFE, 0x07])
        };
        let mut buf = Vec::with_capacity(prefix.len() + m.bytes.len() + suffix.len());
        buf.extend_from_slice(prefix);
        buf.extend_from_slice(&m.bytes);
        buf.extend_from_slice(suffix);
        let found = salted
            .scan(&buf)
            .iter()
            .any(|h| h.offset == prefix.len() && h.len == m.bytes.len() && h.kind == m.kind);
        report.members += 1;
        if found {
            report.detected += 1;
        } else {
            *report.missed.entry(m.kind).or_insert(0) += 1;
        }
    }
    report
}

#[cfg(test)]
mod tests {
    // Only synthetic values: MACs use the 02:00:00 prefix, and every MAC-shaped string is built at
    // run time from bytes.
    use super::*;

    const BASE: [u8; 6] = [0x02, 0x00, 0x00, 0xab, 0xcd, 0xfe];
    const UID: [u8; 16] = [
        0x10, 0x21, 0x32, 0x43, 0x54, 0x65, 0x76, 0x87, 0x98, 0xa9, 0xba, 0xcb, 0xdc, 0xed, 0xfe,
        0x0f,
    ];
    const WORD4: u32 = 0x1a2b_3c4d;
    const WORD6_SPARSE: u32 = 0x0000_000f;
    const WORD7: u32 = 0xa5b6_0000;
    const SALT: [u8; 32] = [0x5a; 32];

    fn hex(bytes: &[u8], upper: bool, sep: &str) -> Vec<u8> {
        let parts: Vec<String> = bytes
            .iter()
            .map(|b| {
                if upper {
                    format!("{b:02X}")
                } else {
                    format!("{b:02x}")
                }
            })
            .collect();
        parts.join(sep).into_bytes()
    }

    fn text_forms(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for upper in [false, true] {
            for sep in [":", "-", ""] {
                out.push(hex(bytes, upper, sep));
            }
        }
        out
    }

    fn blk1() -> Vec<u8> {
        let mut b = vec![0u8; EFUSE_SHORT_BLOCK_LEN];
        for (i, byte) in b.iter_mut().take(6).enumerate() {
            *byte = BASE[5 - i];
        }
        b[6] = 0x3c; // unrelated BLK1 content
        b
    }

    fn blk2() -> Vec<u8> {
        let mut b = vec![0u8; EFUSE_LONG_BLOCK_LEN];
        b[..16].copy_from_slice(&UID);
        b[16..20].copy_from_slice(&WORD4.to_le_bytes());
        b[24..28].copy_from_slice(&WORD6_SPARSE.to_le_bytes());
        b[28..32].copy_from_slice(&WORD7.to_le_bytes());
        b
    }

    fn efuse_set() -> SecretSet {
        let mut b = SecretSet::builder();
        b.efuse_block(0, &[0u8; EFUSE_SHORT_BLOCK_LEN]).unwrap();
        b.efuse_block(1, &blk1()).unwrap();
        b.efuse_block(2, &blk2()).unwrap();
        for i in 3..=10 {
            b.efuse_block(i, &[0u8; EFUSE_LONG_BLOCK_LEN]).unwrap();
        }
        b.build()
    }

    fn member(set: &SecretSet, bytes: &[u8]) -> Option<(MemberKind, bool)> {
        set.members()
            .iter()
            .find(|m| m.bytes == bytes)
            .map(|m| (m.kind, m.text_only))
    }

    fn mac_plus(offset: u8, carry: bool) -> [u8; 6] {
        if carry {
            let v = mac_to_u64(BASE) + u64::from(offset);
            u64_to_mac(v)
        } else {
            let mut m = BASE;
            m[5] = m[5].wrapping_add(offset);
            m
        }
    }

    #[test]
    fn efuse_mac_members_cover_every_form() {
        let set = efuse_set();
        for offset in 0..=3u8 {
            for carry in [false, true] {
                let mac = mac_plus(offset, carry);
                let mut rev = mac;
                rev.reverse();
                for order in [mac, rev] {
                    assert_eq!(member(&set, &order), Some((MemberKind::Mac, false)));
                    for t in text_forms(&order) {
                        assert_eq!(member(&set, &t), Some((MemberKind::Mac, false)));
                    }
                }
                for t in text_forms(&mac[3..]) {
                    assert_eq!(member(&set, &t), Some((MemberKind::MacSuffix, true)));
                }
                assert_eq!(
                    member(&set, &mac[3..]),
                    None,
                    "raw suffix is never a member"
                );
            }
        }
        // Derivations +2 and +3 wrap in the last byte and differ from the carry form, so there are
        // 6 distinct MACs with 14 forms each and 6 suffixes with 6 text forms.
        assert_ne!(mac_plus(2, false), mac_plus(2, true));
        let counts = set.count_by_kind();
        assert_eq!(counts.get(&MemberKind::Mac), Some(&84));
        assert_eq!(counts.get(&MemberKind::MacSuffix), Some(&36));
    }

    #[test]
    fn efuse_block_lengths_and_indices_are_checked() {
        let mut b = SecretSet::builder();
        assert_eq!(
            b.efuse_block(1, &[0u8; 23]).err(),
            Some(SecretSetError::BlockLength {
                index: 1,
                expected: 24,
                actual: 23
            })
        );
        assert_eq!(
            b.efuse_block(2, &[0u8; 24]).err(),
            Some(SecretSetError::BlockLength {
                index: 2,
                expected: 32,
                actual: 24
            })
        );
        assert_eq!(
            b.efuse_block(11, &[0u8; 32]).err(),
            Some(SecretSetError::BlockIndex(11))
        );
        b.efuse_block(1, &[0u8; 24]).unwrap();
        b.efuse_block(2, &[0u8; 32]).unwrap();
        let set = b.build();
        assert!(set.is_empty());
        assert!(set.dropped() > 0);
    }

    #[test]
    fn unique_id_members_in_both_orders() {
        let set = efuse_set();
        let mut rev = UID;
        rev.reverse();
        for order in [UID, rev] {
            assert_eq!(member(&set, &order), Some((MemberKind::UniqueId, false)));
            for upper in [false, true] {
                let t = hex(&order, upper, "");
                assert_eq!(member(&set, &t), Some((MemberKind::UniqueId, false)));
            }
        }
        assert_eq!(set.count_by_kind().get(&MemberKind::UniqueId), Some(&6));
    }

    #[test]
    fn calibration_words_dense_kept_sparse_dropped() {
        let set = efuse_set();
        for word in [WORD4, WORD7] {
            for order in [word.to_le_bytes(), word.to_be_bytes()] {
                assert_eq!(member(&set, &order), Some((MemberKind::CalibWord, false)));
                for upper in [false, true] {
                    let t = hex(&order, upper, "");
                    assert_eq!(member(&set, &t), Some((MemberKind::CalibWord, false)));
                }
            }
        }
        assert_eq!(member(&set, &WORD6_SPARSE.to_le_bytes()), None);
        assert_eq!(
            member(&set, &hex(&WORD6_SPARSE.to_be_bytes(), false, "")),
            None
        );
        assert_eq!(set.count_by_kind().get(&MemberKind::CalibWord), Some(&12));
    }

    #[test]
    fn hex_and_base64_helpers() {
        assert_eq!(decode_hex(&hex_string(&UID)), Some(UID.to_vec()));
        assert_eq!(decode_hex("A0b"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(
            encode_hex(&[0xab, 0x01], true, Some(b'-')),
            b"AB-01".to_vec()
        );
        // RFC 4648 section 10 vectors, keeping only fully determined characters.
        assert_eq!(encode_base64_prefix(b"f"), b"Z".to_vec());
        assert_eq!(encode_base64_prefix(b"fo"), b"Zm".to_vec());
        assert_eq!(encode_base64_prefix(b"foo"), b"Zm9v".to_vec());
        assert_eq!(encode_base64_prefix(b"fooba"), b"Zm9vYm".to_vec());
        assert_eq!(encode_base64_prefix(b"foobar"), b"Zm9vYmFy".to_vec());
    }

    #[test]
    fn kind_names_round_trip() {
        for k in MemberKind::ALL {
            assert_eq!(MemberKind::from_name(k.name()), Some(k));
        }
        assert_eq!(MemberKind::from_name("nope"), None);
        assert!(MemberKind::MacSuffix.is_mac() && !MemberKind::CardId.is_mac());
    }

    #[test]
    fn suffix_matches_only_in_text_like_content() {
        let salted = efuse_set().salted(&SALT).unwrap();
        let suffix = hex(&BASE[3..], false, ":");
        let mut text = b"ssid Passport-".to_vec();
        let at = text.len();
        text.extend_from_slice(&suffix);
        text.extend_from_slice(b" ok\n");
        assert!(is_text_like(&text));
        let hit = Hit {
            offset: at,
            len: suffix.len(),
            kind: MemberKind::MacSuffix,
        };
        assert_eq!(salted.scan(&text), vec![hit]);

        let mut binary = vec![0u8, 1, 2];
        binary.extend_from_slice(&suffix);
        binary.push(0);
        assert!(!is_text_like(&binary));
        assert!(salted.scan(&binary).is_empty());
        assert_eq!(
            salted.scan_as(&binary, true),
            vec![Hit { offset: 3, ..hit }]
        );

        // A full MAC text form matches in binary content too; its embedded suffix only in text.
        let full = hex(&BASE, true, "-");
        let mut bin2 = vec![0u8; 5];
        bin2.extend_from_slice(&full);
        bin2.push(0);
        let mac_hit = Hit {
            offset: 5,
            len: full.len(),
            kind: MemberKind::Mac,
        };
        assert_eq!(salted.scan(&bin2), vec![mac_hit]);
        let in_text = salted.scan(&full);
        assert_eq!(in_text.len(), 2);
        assert_eq!(
            (in_text[0].kind, in_text[1].kind),
            (MemberKind::Mac, MemberKind::MacSuffix)
        );
    }

    #[test]
    fn reversed_mac_found_in_binary_record() {
        let salted = efuse_set().salted(&SALT).unwrap();
        let mut rev = mac_plus(2, false);
        rev.reverse();
        let mut record = vec![0x04, 0x3e, 0x0c, 0x02, 0x01, 0x00, 0x00];
        record.extend_from_slice(&rev);
        record.extend_from_slice(&[0x1f, 0x00, 0xc4]);
        assert_eq!(
            salted.scan(&record),
            vec![Hit {
                offset: 7,
                len: 6,
                kind: MemberKind::Mac
            }]
        );
    }

    #[test]
    fn cardid_members_per_chunk() {
        let pattern: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(7).wrapping_add(1));
        let mut window = vec![0xFFu8; 132];
        window[32..64].copy_from_slice(&pattern);
        window[69] = 0x00;
        window[96..128].fill(0x00);
        window[128..132].copy_from_slice(&[1, 2, 3, 4]);
        let mut b = SecretSet::builder();
        b.cardid_window(&window);
        let set = b.build();
        assert_eq!(set.count_by_kind().get(&MemberKind::CardId), Some(&3));
        assert_eq!(member(&set, &pattern), Some((MemberKind::CardId, false)));
        assert_eq!(
            member(&set, &window[64..96]),
            Some((MemberKind::CardId, false))
        );
        assert_eq!(
            member(&set, &window[128..]),
            Some((MemberKind::CardId, false))
        );
        assert_eq!(set.dropped(), 1, "the all-zero chunk is dropped");

        let salted = set.salted(&SALT).unwrap();
        let mut flash = vec![0xFFu8; 256];
        flash[100..132].copy_from_slice(&pattern);
        assert_eq!(
            salted.scan(&flash),
            vec![Hit {
                offset: 100,
                len: 32,
                kind: MemberKind::CardId
            }]
        );
        assert!(salted.scan(&[0xFFu8; 512]).is_empty());
    }

    /// Stored CRC of the generated page headers with sequence number 0 and 1.
    const GEN_CRC_SEQ0: [u8; 4] = [0x84, 0x2d, 0xba, 0xb9];
    const GEN_CRC_SEQ1: [u8; 4] = [0xa3, 0x48, 0x9f, 0x38];

    /// A page header as ESP-IDF `nvs_partition_gen.py` wrote it (synthetic CSV), with the CRC as
    /// the tool stored it, not as this module computes it.
    fn generated_header(state: u32, seq: u32, crc: [u8; 4]) -> [u8; 32] {
        let mut h = [0xFFu8; 32];
        h[..4].copy_from_slice(&state.to_le_bytes());
        h[4..8].copy_from_slice(&seq.to_le_bytes());
        h[8] = 0xFE;
        h[28..].copy_from_slice(&crc);
        h
    }

    fn with_crc(mut h: [u8; 32]) -> [u8; 32] {
        let crc = nvs_crc32(&h[4..28]);
        h[28..].copy_from_slice(&crc.to_le_bytes());
        h
    }

    #[test]
    fn nvs_page_header_check_matches_nvs_partition_gen() {
        let generated = [
            generated_header(0xFFFF_FFFE, 0, GEN_CRC_SEQ0), // fresh partition, page 0
            generated_header(0xFFFF_FFFC, 0, GEN_CRC_SEQ0), // page 0 after it filled up
            generated_header(0xFFFF_FFFE, 1, GEN_CRC_SEQ1), // page 1
        ];
        for h in &generated {
            assert_eq!(nvs_crc32(&h[4..28]).to_le_bytes(), h[28..]);
            assert!(is_nvs_page_header(h));
        }
        let fresh = generated[0];
        let mut bad = fresh;
        bad[31] ^= 0x01;
        assert!(!is_nvs_page_header(&bad), "wrong CRC");
        let mut bad = fresh;
        bad[20] = 0x00;
        assert!(
            !is_nvs_page_header(&with_crc(bad)),
            "reserved byte not 0xFF"
        );
        let mut bad = fresh;
        bad[8] = 0x01;
        assert!(!is_nvs_page_header(&with_crc(bad)), "unknown version");
        for state in [0xFFFF_FFFFu32, 0, 0x1234_5678] {
            let mut bad = fresh;
            bad[..4].copy_from_slice(&state.to_le_bytes());
            assert!(!is_nvs_page_header(&bad), "unknown state {state:#x}");
        }
        assert!(!is_nvs_page_header(&fresh[..31]), "short chunk");
    }

    #[test]
    fn cardid_window_skips_nvs_page_headers_at_page_boundaries() {
        let fresh = generated_header(0xFFFF_FFFE, 0, GEN_CRC_SEQ0);
        let next = generated_header(0xFFFF_FFFE, 1, GEN_CRC_SEQ1);
        let full = generated_header(0xFFFF_FFFC, 0, GEN_CRC_SEQ0);
        let mut bad_crc = next;
        bad_crc[30] ^= 0x10;
        let data: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(29) ^ 0xa5);
        let other: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_mul(53) ^ 0x3c);

        let mut window = vec![0xFFu8; 4 * NVS_PAGE_LEN];
        window[..32].copy_from_slice(&fresh);
        window[0x40..0x60].copy_from_slice(&data);
        window[0x1000..0x1020].copy_from_slice(&next);
        window[0x2000..0x2020].copy_from_slice(&bad_crc);
        window[0x2020..0x2040].copy_from_slice(&full);
        window[0x3000..0x3020].copy_from_slice(&other);
        let mut b = SecretSet::builder();
        b.cardid_window(&window);
        let set = b.build();

        assert_eq!(set.nvs_headers_skipped(), 2);
        assert_eq!(set.dropped(), 0);
        assert_eq!(set.count_by_kind().get(&MemberKind::CardId), Some(&4));
        assert_eq!(member(&set, &fresh), None, "valid header at 0x0");
        assert_eq!(member(&set, &next), None, "valid header at 0x1000");
        let card = Some((MemberKind::CardId, false));
        assert_eq!(member(&set, &bad_crc), card, "wrong CRC at 0x2000");
        assert_eq!(
            member(&set, &full),
            card,
            "valid header off a page boundary"
        );
        assert_eq!(member(&set, &data), card, "entry data after a header");
        assert_eq!(member(&set, &other), card, "non-header data at 0x3000");
        assert!(format!("{set:?}").contains("nvs_headers_skipped: 2"));
        let rebuilt = SecretSetBuilder::from_set(set.clone()).build();
        assert_eq!(rebuilt.nvs_headers_skipped(), 2);

        let mut b = SecretSet::builder();
        b.cardid_window(&window[..NVS_PAGE_LEN]);
        let salted = b.build().salted(&SALT).unwrap();
        let mut nvs_image = vec![0xFFu8; 3 * NVS_PAGE_LEN];
        nvs_image[..32].copy_from_slice(&fresh);
        nvs_image[0x1000..0x1020].copy_from_slice(&next);
        assert!(salted.scan(&nvs_image).is_empty());
        nvs_image[0x2040..0x2060].copy_from_slice(&data);
        assert_eq!(salted.scan(&nvs_image).len(), 1);
    }

    #[test]
    fn runtime_members_nvs_and_nfc() {
        let mut b = SecretSetBuilder::from_set(efuse_set());
        let before = b.clone().build().len();
        let uid = [0x04, 0xa1, 0x22, 0x33, 0x44, 0x55, 0x80];
        let pwd = [0x9a, 0x8b, 0x7c, 0x6d];
        b.nvs_credential(b"synthetic-pass\0");
        b.nvs_credential(b"short");
        b.nfc_uid(&uid);
        b.nfc_pwd(&pwd);
        b.nfc_pack(&[0x12, 0xab]);
        let set = b.build();
        assert!(set.len() > before);

        let v: &[u8] = b"synthetic-pass";
        let cred = Some((MemberKind::NvsCredential, false));
        assert_eq!(member(&set, v), cred);
        assert_eq!(member(&set, b"synthetic-pass\0"), None);
        for upper in [false, true] {
            assert_eq!(member(&set, &hex(v, upper, "")), cred);
        }
        assert_eq!(member(&set, &encode_base64_prefix(v)), cred);
        assert_eq!(member(&set, b"short"), None);
        assert_eq!(
            set.count_by_kind().get(&MemberKind::NvsCredential),
            Some(&4)
        );

        assert_eq!(member(&set, &uid), Some((MemberKind::NfcUid, false)));
        for t in text_forms(&uid) {
            assert_eq!(member(&set, &t), Some((MemberKind::NfcUid, false)));
        }
        assert_eq!(member(&set, &pwd), Some((MemberKind::NfcPwd, false)));
        assert_eq!(
            member(&set, &[0x12, 0xab]),
            None,
            "2 raw bytes are below the minimum"
        );
        for t in text_forms(&[0x12, 0xab]) {
            assert_eq!(member(&set, &t), Some((MemberKind::NfcPack, true)));
        }
    }

    #[test]
    fn salted_scan_hit_and_miss() {
        let mut b = SecretSetBuilder::from_set(efuse_set());
        b.backup_stem("synthetic-backup-0001");
        b.backup_stem("bak");
        let set = b.build();
        assert_eq!(member(&set, b"bak"), None);
        let salted = set.salted(&SALT).unwrap();
        assert_eq!(salted.len(), set.len());
        assert!(salted.window_lens().contains(&17) && salted.window_lens().contains(&32));

        let mut doc = b"efuse: ".to_vec();
        doc.extend_from_slice(&hex(&UID, true, ""));
        doc.extend_from_slice(b"\nfile synthetic-backup-0001.bin\n");
        let hits: Vec<_> = salted
            .scan(&doc)
            .iter()
            .map(|h| (h.kind, h.offset, h.len))
            .collect();
        assert_eq!(
            hits,
            vec![
                (MemberKind::UniqueId, 7, 32),
                (MemberKind::BackupStem, 45, 21)
            ]
        );

        let mut other = BASE;
        other[5] = 0x10;
        assert!(salted.scan(&hex(&other, false, ":")).is_empty());

        let wrong = SaltedSet::from_parts(vec![0x11; 32], salted.entries().to_vec()).unwrap();
        assert!(wrong.scan(&doc).is_empty());
        let reloaded = SaltedSet::from_parts(salted.salt().to_vec(), salted.entries().to_vec());
        assert_eq!(reloaded.as_ref(), Ok(&salted));
        assert_eq!(
            set.salted(&[1u8; 8]).err(),
            Some(SecretSetError::SaltTooShort(8))
        );
    }

    #[test]
    fn self_test_detects_every_member() {
        let mut b = SecretSetBuilder::from_set(efuse_set());
        b.canary(&[0xc3, 0x5e, 0x91, 0x07, 0x6a, 0xd2, 0x48, 0xbf]);
        b.nfc_pack(&[0x12, 0xab]);
        let mut card = [0xFFu8; 64];
        card[3] = 0x42;
        b.cardid_window(&card);
        let set = b.build();
        let salted = set.salted(&SALT).unwrap();
        let report = self_test(&set, &salted);
        assert!(report.passed(), "{report:?}");
        assert_eq!(report.members, set.len());

        let wrong = SaltedSet::from_parts(vec![0x11; 32], salted.entries().to_vec()).unwrap();
        let bad = self_test(&set, &wrong);
        assert_eq!(bad.detected, 0);
        assert_eq!(bad.missed.values().sum::<usize>(), set.len());
        assert!(!bad.passed());
    }

    #[test]
    fn debug_output_never_contains_member_values() {
        let set = efuse_set();
        let salted = set.salted(&SALT).unwrap();
        let mut dumps = vec![
            format!("{set:?}"),
            format!("{salted:?}"),
            format!("{:?}", SecretSetBuilder::from_set(set.clone())),
        ];
        dumps.extend(set.members().iter().map(|m| format!("{m:?}")));
        dumps.extend(salted.entries().iter().map(|e| format!("{e:?}")));
        let needles = [
            hex(&BASE[3..], false, ""),
            hex(&UID[..4], false, ""),
            hex(&salted.entries()[0].hash[..4], false, ""),
        ];
        for d in &dumps {
            for n in &needles {
                assert!(!d.as_bytes().windows(n.len()).any(|w| w == n.as_slice()));
            }
        }
    }

    #[test]
    fn duplicate_bytes_merge_to_binary_capable() {
        let mut b = SecretSet::builder();
        b.nfc_pack(&[0x12, 0xab]); // bare hex text "12ab" is text-only here
        b.nfc_pwd(b"12ab"); // the same 4 bytes as a raw, binary-capable member
        let set = b.build();
        assert_eq!(member(&set, b"12ab"), Some((MemberKind::NfcPack, false)));
    }
}
