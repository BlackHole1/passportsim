//! NVS partition heuristics of the `nvs-credential` rule.
//!
//! Layout facts come from the public ESP-IDF v5.5.3 `nvs_flash` sources (Apache-2.0, facts
//! only) and were checked against an image written by the ESP-IDF `nvs_partition_gen` tool:
//!
//! - a partition is a run of 4096-byte pages; a flash image keeps the partition at a
//!   page-aligned offset, so pages are probed at every multiple of 4096 in the file;
//! - page header (32 bytes): state `u32` (ACTIVE 0xFFFFFFFE, FULL 0xFFFFFFFC, FREEING
//!   0xFFFFFFF8), sequence number `u32`, version `u8` (0xFE, or 0xFF for the old format),
//!   19 reserved bytes of 0xFF, CRC32 `u32` over header bytes 4 to 27;
//! - entry state bitmap (32 bytes from offset 32): 2 bits per entry, entry `i` at bits `2i`
//!   of the bitmap; 0b11 empty, 0b10 written, 0b00 erased;
//! - 126 entries of 32 bytes from offset 64: namespace index `u8`, type `u8`, span `u8`,
//!   chunk index `u8`, CRC32 `u32` over entry bytes 0 to 3 and 8 to 31, key (16 bytes,
//!   NUL-terminated), data (8 bytes); a string or blob item spans `span` entries;
//! - namespace index 0 holds the namespace table: a U8 (type 0x01) entry whose key is the
//!   namespace name and whose first data byte is the namespace index;
//! - CRC32 uses the reflected polynomial 0xEDB88320 with register init 0 and final xor
//!   0xFFFFFFFF (ESP ROM `crc32_le(0xffffffff, ...)`, chained over the parts).
//!
//! Heuristics: a page counts only when its state, version, reserved bytes and header CRC all
//! match; an entry counts only when it is written, its CRC matches and its key is printable
//! ASCII. A counted entry outside the namespace table is a credential when
//!
//! - its namespace is `nvs.net80211` and its key starts with `sta.` or `ap.` (Wi-Fi station
//!   and soft-AP configuration: SSID, password, PMK, AP records);
//! - its namespace is `nimble_bond` (NimBLE bond records `our_sec`, `peer_sec`, `cccd`) or
//!   `bt_config.conf` (Bluedroid bond store `bt_cfg_key*`);
//! - its namespace is `phy` and its key is `cal_mac` (the MAC kept with PHY calibration);
//! - in any namespace, its key contains `pass`, `pswd`, `pwd`, `psk`, `pmk`, `ssid`, `token`,
//!   `secret`, `cred`, `apikey`, `api_key`, `auth`, `cert` or `priv` (case-insensitive).
//!
//! UNVERIFIED: Wi-Fi keys beyond `sta.ssid`, `sta.pswd` and `ap.passwd` (the names the
//! ESP-IDF NVS host tests use) were not checked against the Wi-Fi library, hence the prefix
//! match on `sta.` and `ap.`.

use std::collections::BTreeMap;

/// NVS page size (one flash sector).
pub const PAGE_SIZE: usize = 4096;
const ENTRY_SIZE: usize = 32;
const ENTRY_COUNT: usize = 126;
const BITMAP_START: usize = 32;
const ENTRIES_START: usize = 64;
const KEY_LEN: usize = 16;
const STATE_ACTIVE: u32 = 0xFFFF_FFFE;
const STATE_FULL: u32 = 0xFFFF_FFFC;
const STATE_FREEING: u32 = 0xFFFF_FFF8;
const ENTRY_WRITTEN: u8 = 0b10;
/// Item type of the namespace table entries.
pub const TYPE_U8: u8 = 0x01;

/// Key substrings that mark a credential in any namespace.
const CREDENTIAL_WORDS: &[&str] = &[
    "pass", "pswd", "pwd", "psk", "pmk", "ssid", "token", "secret", "cred", "apikey", "api_key",
    "auth", "cert", "priv",
];

/// CRC32 as NVS computes it, chained over `parts`.
pub fn crc32_le(parts: &[&[u8]]) -> u32 {
    let mut reg = 0u32;
    for part in parts {
        for &byte in *part {
            reg ^= u32::from(byte);
            for _ in 0..8 {
                reg = if reg & 1 != 0 {
                    (reg >> 1) ^ 0xEDB8_8320
                } else {
                    reg >> 1
                };
            }
        }
    }
    reg ^ 0xFFFF_FFFF
}

fn u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Whether a 4096-byte slice is an initialized NVS page with a valid header.
pub fn is_page(page: &[u8]) -> bool {
    page.len() == PAGE_SIZE
        && matches!(u32_le(page, 0), STATE_ACTIVE | STATE_FULL | STATE_FREEING)
        && matches!(page[8], 0xFE | 0xFF)
        && page[9..28].iter().all(|&b| b == 0xFF)
        && u32_le(page, 28) == crc32_le(&[&page[4..28]])
}

/// A written entry with a valid CRC and a printable key.
struct Entry<'a> {
    /// Offset of the entry within its page.
    offset: usize,
    ns: u8,
    kind: u8,
    key: &'a str,
    first_data_byte: u8,
}

fn entries(page: &[u8]) -> Vec<Entry<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < ENTRY_COUNT {
        let state = (page[BITMAP_START + i / 4] >> ((i % 4) * 2)) & 0b11;
        let at = ENTRIES_START + i * ENTRY_SIZE;
        let raw = &page[at..at + ENTRY_SIZE];
        if state == ENTRY_WRITTEN
            && u32_le(raw, 4) == crc32_le(&[&raw[..4], &raw[8..]])
            && let Some(key) = entry_key(raw)
        {
            out.push(Entry {
                offset: at,
                ns: raw[0],
                kind: raw[1],
                key,
                first_data_byte: raw[24],
            });
            // The span is trusted only once the CRC matched; data entries are skipped.
            i += usize::from(raw[2]).max(1);
        } else {
            i += 1;
        }
    }
    out
}

fn entry_key(raw: &[u8]) -> Option<&str> {
    let field = &raw[8..8 + KEY_LEN];
    let len = field.iter().position(|&b| b == 0)?;
    let key = &field[..len];
    if key.is_empty() || !key.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    std::str::from_utf8(key).ok()
}

/// File offsets of credential entries across every NVS page of `bytes`.
pub fn credential_entries(bytes: &[u8]) -> Vec<usize> {
    let pages: Vec<(usize, Vec<Entry<'_>>)> = bytes
        .chunks_exact(PAGE_SIZE)
        .enumerate()
        .filter(|(_, page)| is_page(page))
        .map(|(n, page)| (n * PAGE_SIZE, entries(page)))
        .collect();
    let mut namespaces = BTreeMap::new();
    for entry in pages.iter().flat_map(|(_, list)| list) {
        if entry.ns == 0 && entry.kind == TYPE_U8 {
            namespaces.insert(entry.first_data_byte, entry.key);
        }
    }
    let mut out = Vec::new();
    for (base, list) in &pages {
        for entry in list {
            if entry.ns != 0 && is_credential(namespaces.get(&entry.ns).copied(), entry.key) {
                out.push(base + entry.offset);
            }
        }
    }
    out
}

/// The credential heuristic of the module documentation.
pub fn is_credential(namespace: Option<&str>, key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    let by_namespace = match namespace {
        Some("nvs.net80211") => key.starts_with("sta.") || key.starts_with("ap."),
        Some("nimble_bond" | "bt_config.conf") => true,
        Some("phy") => key == "cal_mac",
        _ => false,
    };
    by_namespace || CREDENTIAL_WORDS.iter().any(|w| key.contains(w))
}

/// Builds one ACTIVE page holding single-entry items `(namespace index, type, key, data)`,
/// for tests.
#[cfg(test)]
pub fn build_page(items: &[(u8, u8, &str, [u8; 8])]) -> Vec<u8> {
    let mut page = vec![0xFF; PAGE_SIZE];
    page[0..4].copy_from_slice(&STATE_ACTIVE.to_le_bytes());
    page[4..8].copy_from_slice(&0u32.to_le_bytes());
    page[8] = 0xFE;
    let crc = crc32_le(&[&page[4..28]]);
    page[28..32].copy_from_slice(&crc.to_le_bytes());
    for (i, (ns, kind, key, data)) in items.iter().enumerate() {
        let at = ENTRIES_START + i * ENTRY_SIZE;
        let raw = &mut page[at..at + ENTRY_SIZE];
        raw[..4].copy_from_slice(&[*ns, *kind, 1, 0xFF]);
        raw[8..8 + KEY_LEN].fill(0);
        raw[8..8 + key.len()].copy_from_slice(key.as_bytes());
        raw[24..].copy_from_slice(data);
        let crc = crc32_le(&[&raw[..4], &raw[8..]]);
        raw[4..8].copy_from_slice(&crc.to_le_bytes());
        // Written is 0b10: clear the low bit of the entry's pair.
        page[BITMAP_START + i / 4] &= !(0b01 << ((i % 4) * 2));
    }
    page
}
