//! Listing an NVS partition: namespaces, keys and types, **never the value of a credential
//! key**.
//!
//! [`list`] has no reveal parameter: a credential value is replaced by [`REDACTED`] before it
//! reaches an [`NvsEntry`], so no caller or rendering bug can print it. A human-confirmed reveal
//! belongs to the command layer, not here. [`credential_values`] returns the bytes only to feed
//! the `pemu-api` secret set, which matches them against output.
//!
//! Format: ESP-IDF `components/nvs_flash`. 4096-byte pages, each a 32-byte header (state,
//! sequence number, version byte, CRC), a 32-byte entry-state bitmap of two bits per entry, and
//! 126 32-byte entries.

use core::fmt;

pub const REDACTED: &str = "<redacted>";

pub const PAGE_SIZE: usize = 4096;
pub const ENTRY_SIZE: usize = 32;
pub const ENTRIES_PER_PAGE: usize = 126;
const BITMAP_OFFSET: usize = 32;
const ENTRY_OFFSET: usize = 64;
/// Longest string this module will decode for display.
const MAX_VALUE: usize = 64;

/// Page state words (`nvs::Page::PageState`).
const PAGE_UNINITIALIZED: u32 = 0xffff_ffff;
const PAGE_ACTIVE: u32 = 0xffff_fffe;
const PAGE_FULL: u32 = 0xffff_fffc;
const PAGE_FREEING: u32 = 0xffff_fff8;
const PAGE_CORRUPT: u32 = 0xffff_fff0;

/// Entry states, two bits each in the bitmap.
const ENTRY_WRITTEN: u8 = 0b10;
const ENTRY_EMPTY: u8 = 0b11;

/// Namespaces whose every key is a credential (Wi-Fi credentials, BLE bond keys, app tokens).
/// A namespace not listed here is still checked key by key.
pub const CREDENTIAL_NAMESPACES: &[&str] = &[
    "nvs.net80211",
    "wifi",
    "wifi_cfg",
    "bt_cfg",
    "bt_sec",
    "nimble_bond",
    "ble_bond",
];

/// Lower-case markers that make a key a credential in any namespace.
pub const CREDENTIAL_KEY_MARKERS: &[&str] = &[
    "pass", "pwd", "psk", "key", "secret", "token", "cred", "ssid", "bssid", "pmk", "sae", "bond",
    "ltk", "irk", "csrk", "auth", "cert", "priv",
];

/// Value type of an entry (`nvs::ItemType`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum NvsType {
    /// `u8`, `u16`, `u32` or `u64`.
    Unsigned(u8),
    /// `i8`, `i16`, `i32` or `i64`.
    Signed(u8),
    /// NUL-terminated string.
    Str,
    /// In any of its three encodings.
    Blob,
    Other(u8),
}

impl NvsType {
    pub fn from_byte(b: u8) -> NvsType {
        match b {
            0x01 => NvsType::Unsigned(1),
            0x02 => NvsType::Unsigned(2),
            0x04 => NvsType::Unsigned(4),
            0x08 => NvsType::Unsigned(8),
            0x11 => NvsType::Signed(1),
            0x12 => NvsType::Signed(2),
            0x14 => NvsType::Signed(4),
            0x18 => NvsType::Signed(8),
            0x21 => NvsType::Str,
            0x41 | 0x42 | 0x48 => NvsType::Blob,
            other => NvsType::Other(other),
        }
    }

    pub fn tag(self) -> String {
        match self {
            NvsType::Unsigned(n) => format!("u{}", n * 8),
            NvsType::Signed(n) => format!("i{}", n * 8),
            NvsType::Str => "str".into(),
            NvsType::Blob => "blob".into(),
            NvsType::Other(b) => format!("type{b:#04x}"),
        }
    }
}

/// What a listing may show for an entry's value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum NvsValue {
    Number(String),
    /// Escaped and capped at [`MAX_VALUE`] characters.
    Text(String),
    /// Its length only, never its bytes.
    Blob {
        len: u32,
    },
    /// The bytes are not carried.
    Redacted,
}

impl fmt::Display for NvsValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NvsValue::Number(n) => write!(f, "{n}"),
            NvsValue::Text(t) => write!(f, "{t:?}"),
            NvsValue::Blob { len } => write!(f, "<{len} bytes>"),
            NvsValue::Redacted => write!(f, "{REDACTED}"),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NvsEntry {
    /// `ns<index>` when the namespace entry was not found.
    pub namespace: String,
    /// At most 15 characters.
    pub key: String,
    pub ty: NvsType,
    /// Declared size for a string or blob; the width in bytes for a number.
    pub size: u32,
    pub value: NvsValue,
    pub credential: bool,
}

impl NvsEntry {
    pub fn render(&self) -> String {
        format!(
            "{}/{} {} = {}",
            self.namespace,
            self.key,
            self.ty.tag(),
            self.value
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NvsPage {
    pub index: usize,
    pub state: &'static str,
    pub seq: u32,
    /// Format version byte (0xFE is v2).
    pub version: u8,
    pub written: usize,
    pub erased: usize,
    pub empty: usize,
}

impl NvsPage {
    pub fn render(&self) -> String {
        format!(
            "page {} {} seq={} v{:#04x} {}w/{}e/{}empty",
            self.index, self.state, self.seq, self.version, self.written, self.erased, self.empty
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct NvsListing {
    pub pages: Vec<NvsPage>,
    /// In (namespace, key) order, so two listings of the same partition diff cleanly.
    pub entries: Vec<NvsEntry>,
    /// In name order.
    pub namespaces: Vec<String>,
}

impl NvsListing {
    /// One line per page, then one per entry.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for p in &self.pages {
            out.push_str(&p.render());
            out.push('\n');
        }
        for e in &self.entries {
            out.push_str(&e.render());
            out.push('\n');
        }
        out
    }

    pub fn credentials(&self) -> impl Iterator<Item = &NvsEntry> {
        self.entries.iter().filter(|e| e.credential)
    }
}

pub fn is_credential(namespace: &str, key: &str) -> bool {
    let ns = namespace.to_ascii_lowercase();
    if CREDENTIAL_NAMESPACES.iter().any(|n| ns == *n) {
        return true;
    }
    let k = key.to_ascii_lowercase();
    CREDENTIAL_KEY_MARKERS.iter().any(|m| k.contains(m))
}

struct RawEntry {
    ns_index: u8,
    key: String,
    ty: NvsType,
    size: u32,
    payload: Option<Vec<u8>>,
}

/// One pass over a whole partition. [`list`] and [`credential_values`] must share it: ESP-IDF
/// routinely puts `nvs.net80211 -> 2` on page 0 and the `sta.pswd` item it names on page 1, and a
/// per-page pass would leave that password outside [`CREDENTIAL_NAMESPACES`].
struct Scan {
    pages: Vec<NvsPage>,
    /// Sorted.
    namespaces: Vec<(u8, String)>,
    entries: Vec<RawEntry>,
}

impl Scan {
    fn namespace(&self, index: u8) -> String {
        self.namespaces
            .iter()
            .find(|(i, _)| *i == index)
            .map(|(_, n)| n.clone())
            .unwrap_or_else(|| format!("ns{index}"))
    }
}

fn scan(partition: &[u8]) -> Scan {
    let mut pages: Vec<NvsPage> = Vec::new();
    let mut namespaces: Vec<(u8, String)> = Vec::new();
    let mut raw: Vec<RawEntry> = Vec::new();
    for (index, page) in partition.chunks(PAGE_SIZE).enumerate() {
        if page.len() < ENTRY_OFFSET {
            break;
        }
        let state = u32::from_le_bytes([page[0], page[1], page[2], page[3]]);
        let mut summary = NvsPage {
            index,
            state: page_state(state),
            seq: u32::from_le_bytes([page[4], page[5], page[6], page[7]]),
            version: page[8],
            written: 0,
            erased: 0,
            empty: 0,
        };
        // The bitmap is the authority on written, erased and empty counts.
        for i in 0..ENTRIES_PER_PAGE {
            match entry_state(page, i) {
                ENTRY_EMPTY => summary.empty += 1,
                ENTRY_WRITTEN => summary.written += 1,
                _ => summary.erased += 1,
            }
        }
        pages.push(summary);
        let mut i = 0usize;
        while i < ENTRIES_PER_PAGE {
            let at = ENTRY_OFFSET + i * ENTRY_SIZE;
            let Some(entry) = page.get(at..at + ENTRY_SIZE) else {
                break;
            };
            let span = usize::from(entry[2]).max(1);
            if entry_state(page, i) != ENTRY_WRITTEN {
                i += 1;
                continue;
            }
            let ns_index = entry[0];
            let ty = NvsType::from_byte(entry[1]);
            let key = c_str(&entry[8..24]);
            let (size, payload) = match ty {
                NvsType::Str | NvsType::Blob => {
                    let len = u32::from(u16::from_le_bytes([entry[24], entry[25]]));
                    let start = at + ENTRY_SIZE;
                    let end = start + (len as usize).min(span.saturating_sub(1) * ENTRY_SIZE);
                    (len, page.get(start..end).map(<[u8]>::to_vec))
                }
                NvsType::Unsigned(n) | NvsType::Signed(n) => {
                    (u32::from(n), Some(entry[24..32].to_vec()))
                }
                // An unknown type is shown by length only, but its inline bytes are still carried
                // so a credential of that type reaches the secret set.
                NvsType::Other(_) => (0, Some(entry[24..32].to_vec())),
            };
            if ns_index == 0 && matches!(ty, NvsType::Unsigned(1)) {
                // A namespace entry: the key is the name, the value the index.
                namespaces.push((entry[24], key));
            } else {
                raw.push(RawEntry {
                    ns_index,
                    key,
                    ty,
                    size,
                    payload,
                });
            }
            i += span;
        }
    }
    namespaces.sort();
    Scan {
        pages,
        namespaces,
        entries: raw,
    }
}

/// Lists an NVS partition, with every credential value redacted.
pub fn list(partition: &[u8]) -> NvsListing {
    let scan = scan(partition);
    let mut out = NvsListing {
        pages: scan.pages.clone(),
        ..NvsListing::default()
    };
    for RawEntry {
        ns_index,
        key,
        ty,
        size,
        payload,
    } in &scan.entries
    {
        let namespace = scan.namespace(*ns_index);
        let credential = is_credential(&namespace, key);
        let value = if credential {
            NvsValue::Redacted
        } else {
            show(*ty, *size, payload.as_deref())
        };
        out.entries.push(NvsEntry {
            namespace,
            key: key.clone(),
            ty: *ty,
            size: *size,
            value,
            credential,
        });
    }
    out.entries
        .sort_by(|a, b| (&a.namespace, &a.key).cmp(&(&b.namespace, &b.key)));
    out.namespaces = scan.namespaces.into_iter().map(|(_, n)| n).collect();
    out.namespaces.sort();
    out.namespaces.dedup();
    out
}

/// The raw bytes of every credential value, **for the `pemu-api` secret set only**, which
/// redacts them by value. Nothing may render the result; [`list`] is for anything an agent sees.
pub fn credential_values(partition: &[u8]) -> Vec<Vec<u8>> {
    let scan = scan(partition);
    let mut out = Vec::new();
    for entry in &scan.entries {
        if !is_credential(&scan.namespace(entry.ns_index), &entry.key) {
            continue;
        }
        if let Some(bytes) = &entry.payload {
            out.push(bytes.clone());
        }
    }
    out
}

fn show(ty: NvsType, size: u32, payload: Option<&[u8]>) -> NvsValue {
    match (ty, payload) {
        (NvsType::Unsigned(n), Some(b)) => NvsValue::Number(le_u64(b, n as usize).to_string()),
        (NvsType::Signed(n), Some(b)) => {
            let raw = le_u64(b, n as usize);
            let bits = u32::from(n) * 8;
            let signed = if bits < 64 && raw & (1 << (bits - 1)) != 0 {
                (raw as i64) - (1i64 << bits)
            } else {
                raw as i64
            };
            NvsValue::Number(signed.to_string())
        }
        (NvsType::Str, Some(b)) => {
            let text = c_str(b);
            NvsValue::Text(text.chars().take(MAX_VALUE).collect())
        }
        (NvsType::Blob, _) => NvsValue::Blob { len: size },
        _ => NvsValue::Blob { len: size },
    }
}

fn le_u64(bytes: &[u8], n: usize) -> u64 {
    let mut v = 0u64;
    for (i, b) in bytes.iter().take(n).enumerate() {
        v |= u64::from(*b) << (8 * i);
    }
    v
}

fn entry_state(page: &[u8], i: usize) -> u8 {
    match page.get(BITMAP_OFFSET + i / 4) {
        Some(byte) => (byte >> ((i % 4) * 2)) & 0b11,
        None => ENTRY_EMPTY,
    }
}

fn page_state(state: u32) -> &'static str {
    match state {
        PAGE_UNINITIALIZED => "uninitialized",
        PAGE_ACTIVE => "active",
        PAGE_FULL => "full",
        PAGE_FREEING => "freeing",
        PAGE_CORRUPT => "corrupt",
        _ => "unknown",
    }
}

/// Bytes up to the first NUL, decoded lossily, with control characters replaced.
fn c_str(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end])
        .chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WIFI_PASSWORD: &str = "correct-horse-battery";
    const APP_TOKEN: &str = "tok-0123456789";

    fn entry(page: &mut [u8], i: usize, ns: u8, ty: u8, span: u8, key: &str, data: &[u8; 8]) {
        let at = ENTRY_OFFSET + i * ENTRY_SIZE;
        page[at] = ns;
        page[at + 1] = ty;
        page[at + 2] = span;
        page[at + 8..at + 8 + key.len()].copy_from_slice(key.as_bytes());
        page[at + 24..at + 32].copy_from_slice(data);
    }

    fn payload(page: &mut [u8], i: usize, bytes: &[u8]) {
        let at = ENTRY_OFFSET + (i + 1) * ENTRY_SIZE;
        page[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// Marks entries `0..written` written, the next `erased` erased, and leaves the rest empty.
    fn bitmap(page: &mut [u8], written: usize, erased: usize) {
        for i in 0..ENTRIES_PER_PAGE {
            let state = if i < written {
                ENTRY_WRITTEN
            } else if i < written + erased {
                0b00
            } else {
                ENTRY_EMPTY
            };
            let byte = &mut page[BITMAP_OFFSET + i / 4];
            *byte = (*byte & !(0b11 << ((i % 4) * 2))) | (state << ((i % 4) * 2));
        }
    }

    fn partition() -> Vec<u8> {
        let mut page = vec![0xffu8; PAGE_SIZE];
        page[0..4].copy_from_slice(&PAGE_ACTIVE.to_le_bytes());
        page[4..8].copy_from_slice(&1u32.to_le_bytes());
        page[8] = 0xfe; // NVS format v2
        page[ENTRY_OFFSET..].fill(0);
        // Namespace entries: index 0, type u8, key is the name, value the index.
        entry(
            &mut page,
            0,
            0,
            0x01,
            1,
            "game_prefs",
            &[1, 0, 0, 0, 0, 0, 0, 0],
        );
        entry(
            &mut page,
            1,
            0,
            0x01,
            1,
            "nvs.net80211",
            &[2, 0, 0, 0, 0, 0, 0, 0],
        );
        entry(
            &mut page,
            2,
            1,
            0x01,
            1,
            "volume",
            &[7, 0, 0, 0, 0, 0, 0, 0],
        );
        entry(
            &mut page,
            3,
            1,
            0x14,
            1,
            "top_score",
            &[0xf8, 0xff, 0xff, 0xff, 0, 0, 0, 0],
        );
        let mut var = |i: usize, ns: u8, key: &str, text: &str| {
            let len = (text.len() + 1) as u16;
            let mut data = [0u8; 8];
            data[0..2].copy_from_slice(&len.to_le_bytes());
            entry(&mut page, i, ns, 0x21, 2, key, &data);
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            payload(&mut page, i, &bytes);
        };
        var(4, 1, "language", "en");
        // A password in a credential namespace, and a token matched by key marker.
        var(6, 2, "sta.pwd", WIFI_PASSWORD);
        var(8, 1, "api_token", APP_TOKEN);
        bitmap(&mut page, 10, 1);
        page
    }

    #[test]
    fn a_credential_value_never_appears_in_a_listing() {
        let listing = list(&partition());
        let text = listing.render();
        assert!(!text.contains(WIFI_PASSWORD), "{text}");
        assert!(!text.contains(APP_TOKEN), "{text}");
        // Nor in the Debug form a log line might print.
        for e in &listing.entries {
            let rendered = format!("{} {:?}", e.render(), e);
            assert!(!rendered.contains(WIFI_PASSWORD));
            assert!(!rendered.contains(APP_TOKEN));
        }
        assert_eq!(
            text,
            "page 0 active seq=1 v0xfe 10w/1e/115empty\n\
             game_prefs/api_token str = <redacted>\n\
             game_prefs/language str = \"en\"\n\
             game_prefs/top_score i32 = -8\n\
             game_prefs/volume u8 = 7\n\
             nvs.net80211/sta.pwd str = <redacted>\n"
        );
        assert_eq!(listing.namespaces, ["game_prefs", "nvs.net80211"]);
        let credentials: Vec<&str> = listing.credentials().map(|e| e.key.as_str()).collect();
        assert_eq!(credentials, ["api_token", "sta.pwd"]);
        assert!(listing.credentials().all(|e| e.value == NvsValue::Redacted));
    }

    #[test]
    fn credentials_are_classed_by_namespace_and_by_key_marker() {
        assert!(is_credential("nvs.net80211", "anything"));
        assert!(is_credential("NVS.NET80211", "anything"));
        assert!(is_credential("app", "api_token"));
        assert!(is_credential("app", "WIFI_PASSWORD"));
        assert!(is_credential("app", "ble_bond_ltk"));
        assert!(is_credential("app", "device_key"));
        assert!(!is_credential("game_prefs", "volume"));
        assert!(!is_credential("game_prefs", "language"));
    }

    #[test]
    fn credential_values_feed_the_secret_set_and_nothing_else() {
        let partition = partition();
        let values = credential_values(&partition);
        let text: Vec<String> = values
            .iter()
            .map(|v| {
                String::from_utf8_lossy(v)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .collect();
        assert!(text.iter().any(|v| v == WIFI_PASSWORD));
        assert!(text.iter().any(|v| v == APP_TOKEN));
        assert!(!text.iter().any(|v| v == "en"));
        // The secret set only takes values of 6 or more bytes.
        assert!(values.iter().all(|v| v.len() >= 6));
    }

    /// `sta.pswd` matches no key marker, so only a namespace entry on the previous page classes
    /// it.
    #[test]
    fn a_credential_is_classed_by_a_namespace_declared_on_an_earlier_page() {
        assert!(
            !CREDENTIAL_KEY_MARKERS
                .iter()
                .any(|m| "sta.pswd".contains(m)),
            "the canonical ESP-IDF Wi-Fi password key matches no key marker"
        );
        let mut first = vec![0xffu8; PAGE_SIZE];
        first[0..4].copy_from_slice(&PAGE_FULL.to_le_bytes());
        first[4..8].copy_from_slice(&0u32.to_le_bytes());
        first[8] = 0xfe;
        first[ENTRY_OFFSET..].fill(0);
        entry(
            &mut first,
            0,
            0,
            0x01,
            1,
            "nvs.net80211",
            &[2, 0, 0, 0, 0, 0, 0, 0],
        );
        bitmap(&mut first, 1, 0);

        let mut second = vec![0xffu8; PAGE_SIZE];
        second[0..4].copy_from_slice(&PAGE_ACTIVE.to_le_bytes());
        second[4..8].copy_from_slice(&1u32.to_le_bytes());
        second[8] = 0xfe;
        second[ENTRY_OFFSET..].fill(0);
        let mut data = [0u8; 8];
        data[0..2].copy_from_slice(&((WIFI_PASSWORD.len() + 1) as u16).to_le_bytes());
        entry(&mut second, 0, 2, 0x21, 2, "sta.pswd", &data);
        let mut bytes = WIFI_PASSWORD.as_bytes().to_vec();
        bytes.push(0);
        payload(&mut second, 0, &bytes);
        bitmap(&mut second, 2, 0);

        let mut partition = first;
        partition.extend_from_slice(&second);

        let listing = list(&partition);
        assert_eq!(
            listing.render(),
            "page 0 full seq=0 v0xfe 1w/0e/125empty\n\
             page 1 active seq=1 v0xfe 2w/0e/124empty\n\
             nvs.net80211/sta.pswd str = <redacted>\n"
        );
        assert!(!listing.render().contains(WIFI_PASSWORD));
        let values: Vec<String> = credential_values(&partition)
            .iter()
            .map(|v| {
                String::from_utf8_lossy(v)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .collect();
        assert_eq!(values, [WIFI_PASSWORD]);
    }

    /// Exported partitions have erased NVS pages, so blank and erased pages must list, not be
    /// refused.
    #[test]
    fn blank_erased_and_truncated_partitions_list_cleanly() {
        let blank = vec![0xffu8; PAGE_SIZE * 2];
        let listing = list(&blank);
        assert_eq!(listing.entries, []);
        assert_eq!(listing.pages.len(), 2);
        assert_eq!(listing.pages[0].state, "uninitialized");
        assert_eq!(listing.pages[0].empty, ENTRIES_PER_PAGE);
        assert_eq!(
            listing.render(),
            "page 0 uninitialized seq=4294967295 v0xff 0w/0e/126empty\n\
             page 1 uninitialized seq=4294967295 v0xff 0w/0e/126empty\n"
        );
        assert_eq!(list(&[0xff; 8]).pages, []);
        assert_eq!(list(&[]).pages, []);
        assert_eq!(credential_values(&blank), Vec::<Vec<u8>>::new());
    }

    #[test]
    fn a_blob_is_listed_by_length_only() {
        let mut page = vec![0u8; PAGE_SIZE];
        page[0..4].copy_from_slice(&PAGE_FULL.to_le_bytes());
        page[8] = 0xfe;
        entry(
            &mut page,
            0,
            0,
            0x01,
            1,
            "game_prefs",
            &[1, 0, 0, 0, 0, 0, 0, 0],
        );
        let mut data = [0u8; 8];
        data[0..2].copy_from_slice(&40u16.to_le_bytes());
        entry(&mut page, 1, 1, 0x42, 3, "scores", &data);
        payload(&mut page, 1, b"first-place-name-that-is-not-a-secret-ok");
        bitmap(&mut page, 4, 0);
        let listing = list(&page);
        assert_eq!(
            listing.render(),
            "page 0 full seq=0 v0xfe 4w/0e/122empty\n\
             game_prefs/scores blob = <40 bytes>\n"
        );
        assert!(!listing.render().contains("first-place"));
    }
}
