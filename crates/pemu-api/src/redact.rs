//! The redaction pass over the [`crate::secret_set`] builder.
//!
//! It runs on every agent-visible output and artifact file and matches by value, not by shape: each
//! machine's [`SecretSet`] holds every rendering of its own values. Masking MAC shapes would also
//! erase a BSSID or peer address the agent supplied and must see back.
//!
//! | Member kind | Replaced with | Revealed by |
//! |---|---|---|
//! | MAC, MAC suffix, unique id, calibration word | `<MAC>` for the two MAC kinds, else `<SECRET>` | `--reveal identity`, with human confirmation |
//! | NVS credential value | `<SECRET>` | `inspect nvs --reveal`, with a human confirmation code |
//! | NFC UID, PWD, PACK | `<SECRET>` | `--include-secrets` |
//! | cardid content, device backup stem, guard canary | `<SECRET>` | never |
//!
//! Matching is longest-first at each offset. Binary artifacts keep their length: each match becomes
//! [`BINARY_FILL`]. The pass runs before shaping, never after: a line cap that cuts a MAC leaves a
//! prefix that no longer matches.

use std::collections::BTreeMap;

use crate::error::ApiError;
use crate::output::Output;
use crate::secret_set::{MemberKind, SecretSet, is_text_like, salted_hash};
use crate::shape::{
    Cursor, ResetMark, SerialExcerpt, ShapeLimits, Shaped, serial_chunk, shape_within,
};

pub const MAC_TOKEN: &str = "<MAC>";

pub const SECRET_TOKEN: &str = "<SECRET>";

/// `<` and `>` break the artifact name rule and Windows file names.
pub const PATH_TOKEN: &str = "redacted";

/// The erased flash state, which no member can match.
pub const BINARY_FILL: u8 = 0xFF;

/// Opt-ins a human granted the caller; this type only records them, the CLI confirms.
/// [`Reveal::NONE`] is what every agent-facing surface uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reveal {
    pub identity: bool,
    pub nvs: bool,
    pub include_secrets: bool,
}

impl Reveal {
    pub const NONE: Reveal = Reveal {
        identity: false,
        nvs: false,
        include_secrets: false,
    };

    #[must_use]
    pub const fn with_identity(mut self) -> Reveal {
        self.identity = true;
        self
    }

    #[must_use]
    pub const fn with_nvs(mut self) -> Reveal {
        self.nvs = true;
        self
    }

    #[must_use]
    pub const fn with_include_secrets(mut self) -> Reveal {
        self.include_secrets = true;
        self
    }

    /// Such opt-ins can never be reached from MCP or HTTP.
    #[must_use]
    pub const fn needs_human_confirmation(self) -> bool {
        self.identity || self.nvs || self.include_secrets
    }

    /// Cardid content, backup stems and the guard canary have no opt-in at all.
    #[must_use]
    pub const fn allows(self, kind: MemberKind) -> bool {
        match kind {
            MemberKind::Mac
            | MemberKind::MacSuffix
            | MemberKind::UniqueId
            | MemberKind::CalibWord => self.identity,
            MemberKind::NvsCredential => self.nvs,
            MemberKind::NfcUid | MemberKind::NfcPwd | MemberKind::NfcPack => self.include_secrets,
            MemberKind::CardId | MemberKind::BackupStem | MemberKind::Canary => false,
        }
    }
}

/// What one pass replaced, by kind. Never values or offsets: a count is all a caller may learn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionReport {
    pub replaced: usize,
    pub by_kind: BTreeMap<MemberKind, usize>,
}

impl RedactionReport {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.replaced == 0
    }

    pub fn merge(&mut self, other: &RedactionReport) {
        self.replaced += other.replaced;
        for (kind, count) in &other.by_kind {
            *self.by_kind.entry(*kind).or_insert(0) += count;
        }
    }

    fn record(&mut self, kind: MemberKind) {
        self.replaced += 1;
        *self.by_kind.entry(kind).or_insert(0) += 1;
    }
}

/// Build it once per output: the constructor indexes the set by member length.
#[derive(Clone, Debug)]
pub struct Redactor<'a> {
    reveal: Reveal,
    /// Only the kinds this [`Reveal`] does not allow.
    members: BTreeMap<&'a [u8], (MemberKind, bool)>,
    lens: Vec<usize>,
    binary_lens: Vec<usize>,
}

impl<'a> Redactor<'a> {
    #[must_use]
    pub fn new(set: &'a SecretSet) -> Redactor<'a> {
        Redactor::with_reveal(set, Reveal::NONE)
    }

    #[must_use]
    pub fn with_reveal(set: &'a SecretSet, reveal: Reveal) -> Redactor<'a> {
        let mut members = BTreeMap::new();
        let mut lens = Vec::new();
        let mut binary_lens = Vec::new();
        for member in set.members() {
            if reveal.allows(member.kind) {
                continue;
            }
            members.insert(member.bytes.as_slice(), (member.kind, member.text_only));
            if !lens.contains(&member.bytes.len()) {
                lens.push(member.bytes.len());
            }
            if !member.text_only && !binary_lens.contains(&member.bytes.len()) {
                binary_lens.push(member.bytes.len());
            }
        }
        lens.sort_unstable_by(|a, b| b.cmp(a));
        binary_lens.sort_unstable_by(|a, b| b.cmp(a));
        Redactor {
            reveal,
            members,
            lens,
            binary_lens,
        }
    }

    #[must_use]
    pub fn reveal(&self) -> Reveal {
        self.reveal
    }

    /// An untainted machine's set is empty by construction.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    fn match_at(&self, data: &[u8], at: usize, text_like: bool) -> Option<(usize, MemberKind)> {
        let lens = if text_like {
            &self.lens
        } else {
            &self.binary_lens
        };
        for &len in lens {
            if at + len > data.len() {
                continue;
            }
            if let Some((kind, text_only)) = self.members.get(&data[at..at + len])
                && (text_like || !text_only)
            {
                return Some((len, *kind));
            }
        }
        None
    }

    #[must_use]
    pub fn redact_text(&self, text: &str) -> String {
        self.redact_text_counted(text).0
    }

    #[must_use]
    pub fn redact_text_counted(&self, text: &str) -> (String, RedactionReport) {
        self.replace_matches(text, false)
    }

    /// Uses [`PATH_TOKEN`] so the result is still a valid artifact path: a tainted instance
    /// directory may be named after a MAC.
    #[must_use]
    pub fn redact_path_counted(&self, path: &str) -> (String, RedactionReport) {
        self.replace_matches(path, true)
    }

    fn replace_matches(&self, text: &str, path_safe: bool) -> (String, RedactionReport) {
        let mut report = RedactionReport::default();
        if self.is_empty() {
            return (text.to_string(), report);
        }
        let data = text.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(data.len());
        let mut at = 0;
        while at < data.len() {
            match self.match_at(data, at, true) {
                Some((len, kind)) => {
                    let replacement = if path_safe { PATH_TOKEN } else { token(kind) };
                    out.extend_from_slice(replacement.as_bytes());
                    report.record(kind);
                    at += len;
                }
                None => {
                    out.push(data[at]);
                    at += 1;
                }
            }
        }
        // A raw-byte member can end inside a multi-byte scalar, so the result is repaired.
        (String::from_utf8_lossy(&out).into_owned(), report)
    }

    /// Redacts, then shapes. One call because shaping first could bisect a secret and leave a
    /// prefix no later pass can match.
    #[must_use]
    pub fn shape_text(&self, text: &str, limits: &ShapeLimits) -> Shaped {
        shape_within(&self.redact_text(text), limits)
    }

    /// Redacts a channel chunk and shapes it. The cursor stays on the raw bytes, since it names an
    /// offset in the channel's own stream; only the visible text is masked.
    #[must_use]
    pub fn shape_serial(
        &self,
        chunk: &[u8],
        from: Cursor,
        resets: &[ResetMark],
        limits: &ShapeLimits,
    ) -> SerialExcerpt {
        let (text, mut excerpt) = serial_chunk(chunk, from, resets);
        excerpt.shaped = self.shape_text(&text, limits);
        excerpt
    }

    /// Overwrites every match with [`BINARY_FILL`], keeping the length. Text-only members match
    /// only in text-like content, so a two-byte value's hex form does not shred a pcap.
    #[must_use]
    pub fn redact_bytes(&self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.redact_bytes_in_place(&mut out);
        out
    }

    pub fn redact_bytes_in_place(&self, data: &mut [u8]) -> RedactionReport {
        let mut report = RedactionReport::default();
        if self.is_empty() {
            return report;
        }
        let text_like = is_text_like(data);
        let mut at = 0;
        while at < data.len() {
            match self.match_at(data, at, text_like) {
                Some((len, kind)) => {
                    data[at..at + len].fill(BINARY_FILL);
                    report.record(kind);
                    at += len;
                }
                None => at += 1,
            }
        }
        report
    }

    /// Keys included: an NVS namespace can be named after a MAC.
    pub fn redact_json(&self, value: &mut serde_json::Value) -> RedactionReport {
        let mut report = RedactionReport::default();
        self.redact_json_into(value, &mut report);
        report
    }

    fn redact_json_into(&self, value: &mut serde_json::Value, report: &mut RedactionReport) {
        match value {
            serde_json::Value::String(text) => {
                let (redacted, found) = self.redact_text_counted(text);
                if !found.is_empty() {
                    *text = redacted;
                    report.merge(&found);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_json_into(item, report);
                }
            }
            serde_json::Value::Object(map) => {
                // Rebuilt rather than edited in place: two sibling keys can redact to the same
                // token, and re-inserting would delete a row.
                let mut rebuilt = serde_json::Map::with_capacity(map.len());
                for (key, mut item) in std::mem::take(map) {
                    self.redact_json_into(&mut item, report);
                    let (redacted_key, found) = self.redact_text_counted(&key);
                    let key = if found.is_empty() {
                        key
                    } else {
                        report.merge(&found);
                        redacted_key
                    };
                    rebuilt.insert(free_key(&rebuilt, key), item);
                }
                *map = rebuilt;
            }
            _ => {}
        }
    }

    /// JSON, text, artifact paths and receipt. Artifact file bytes go through
    /// [`Redactor::redact_bytes`] where they are written.
    pub fn redact_output(&self, output: &mut Output) -> RedactionReport {
        let mut report = self.redact_json(&mut output.json);
        let (text, found) = self.redact_text_counted(&output.text);
        output.text = text;
        report.merge(&found);
        for artifact in &mut output.artifacts {
            let (path, found) = self.redact_path_counted(&artifact.path);
            artifact.path = path;
            report.merge(&found);
        }
        let mut receipt_json = output.receipt.to_json();
        let found = self.redact_json(&mut receipt_json);
        if !found.is_empty()
            && let Some(receipt) = crate::receipt::Receipt::from_json(&receipt_json)
        {
            output.receipt = receipt;
            report.merge(&found);
        }
        report
    }

    /// The error envelope is as agent-visible as an output, and `serial_tail` is exactly where a
    /// MAC or NVS value gets printed. The backtrace holds symbol names and normalized paths, so it
    /// is left alone.
    pub fn redact_error(&self, error: &mut ApiError) -> RedactionReport {
        let mut report = RedactionReport::default();
        let (message, found) = self.redact_text_counted(&error.message);
        error.message = message;
        report.merge(&found);
        if let Some(hint) = &error.hint {
            let (redacted, found) = self.redact_text_counted(hint);
            if !found.is_empty() {
                error.hint = Some(redacted.into_boxed_str());
                report.merge(&found);
            }
        }
        report.merge(&self.redact_json(&mut error.detail));
        let mut lines: Vec<String> = Vec::with_capacity(error.serial_tail.len());
        for line in &error.serial_tail {
            let (redacted, found) = self.redact_text_counted(line);
            lines.push(redacted);
            report.merge(&found);
        }
        error.serial_tail = lines.into_boxed_slice();
        report
    }
}

/// `key`, else `key.1`, `key.2` and so on, so two keys that redact to the same token both survive.
fn free_key(map: &serde_json::Map<String, serde_json::Value>, key: String) -> String {
    if !map.contains_key(&key) {
        return key;
    }
    for n in 1..=u32::MAX {
        let candidate = format!("{key}.{n}");
        if !map.contains_key(&candidate) {
            return candidate;
        }
    }
    key
}

#[must_use]
pub fn token(kind: MemberKind) -> &'static str {
    if kind.is_mac() {
        MAC_TOKEN
    } else {
        SECRET_TOKEN
    }
}

/// `Redacted{<salted sha256 in hex>}`. Salted so the label cannot be looked up in a table of
/// candidate values; `xtask secrets-check --init` keeps the salt outside the repository.
#[must_use]
pub fn redacted_label(salt: &[u8], bytes: &[u8]) -> String {
    let hash = salted_hash(salt, bytes);
    let mut out = String::with_capacity(9 + 64);
    out.push_str("Redacted{");
    for byte in hash {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out.push('}');
    out
}

/// Fills an exported cardid window with 0xFF and returns the label recording it; the hash is taken
/// before the fill.
pub fn redact_cardid_window(window: &mut [u8], salt: &[u8]) -> String {
    let label = redacted_label(salt, window);
    window.fill(BINARY_FILL);
    label
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::Receipt;
    use crate::secret_set::encode_hex;

    /// Synthetic, with the `02:00:00` placeholder prefix the commit hook requires.
    const MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x12, 0x34, 0x56];
    const UID: [u8; 16] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x0f,
        0x10,
    ];
    const CREDENTIAL: &[u8] = b"correct horse battery";
    /// Printable, so the text path can be tested too.
    fn cardid_chunk() -> Vec<u8> {
        (0..32u8).map(|i| b'@' + i).collect()
    }

    fn hex(bytes: &[u8], sep: Option<u8>) -> String {
        String::from_utf8(encode_hex(bytes, false, sep)).expect("hex is ASCII")
    }

    fn reversed(bytes: &[u8]) -> Vec<u8> {
        let mut out = bytes.to_vec();
        out.reverse();
        out
    }

    fn full_set() -> SecretSet {
        let mut builder = SecretSet::builder();
        builder
            .base_mac(MAC)
            .unique_id(UID)
            .calibration_word(0x1234_5678)
            .backup_stem("passport-backup-fixture")
            .cardid_window(&cardid_chunk())
            .nvs_credential(CREDENTIAL)
            .nfc_uid(&[0x04, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6]);
        builder.build()
    }

    #[test]
    fn a_mac_in_every_text_form_becomes_the_mac_token() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        for sep in [Some(b':'), Some(b'-'), None] {
            let line = format!("I (412) wifi: sta mac {} up", hex(&MAC, sep));
            assert_eq!(
                redactor.redact_text(&line),
                "I (412) wifi: sta mac <MAC> up"
            );
        }
        let upper = String::from_utf8(encode_hex(&MAC, true, Some(b':'))).expect("hex is ASCII");
        assert_eq!(redactor.redact_text(&upper), "<MAC>");
    }

    #[test]
    fn a_value_that_appears_byte_reversed_is_redacted() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        // How several drivers print it. Built here so no MAC-shaped literal outside the placeholder
        // prefix enters the tree.
        let backwards = reversed(&MAC);
        for sep in [Some(b':'), Some(b'-'), None] {
            let line = format!("bt addr {} (le)", hex(&backwards, sep));
            assert_eq!(redactor.redact_text(&line), "bt addr <MAC> (le)");
        }
        let mut blob = vec![0u8; 4];
        blob.extend_from_slice(&backwards);
        blob.extend_from_slice(&[0x00; 4]);
        let redacted = redactor.redact_bytes(&blob);
        assert_eq!(&redacted[4..10], &[BINARY_FILL; 6]);
        assert_eq!(redacted.len(), blob.len());
    }

    #[test]
    fn a_reversed_unique_id_and_calibration_word_are_redacted() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let uid = format!("chip uid {}", hex(&reversed(&UID), None));
        assert_eq!(redactor.redact_text(&uid), "chip uid <SECRET>");
        let word = format!("adc cal {}", hex(&0x1234_5678u32.to_be_bytes(), None));
        assert_eq!(redactor.redact_text(&word), "adc cal <SECRET>");
    }

    #[test]
    fn a_derived_mac_is_redacted_as_well_as_the_base() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        // ESP-IDF derives the SoftAP, BT and Ethernet MACs by adding to the last byte.
        for offset in 0..=3u8 {
            let mut derived = MAC;
            derived[5] = derived[5].wrapping_add(offset);
            assert_eq!(redactor.redact_text(&hex(&derived, Some(b':'))), "<MAC>");
        }
    }

    #[test]
    fn the_three_byte_suffix_is_redacted_in_text_only() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let suffix = hex(&MAC[3..], Some(b':'));
        assert_eq!(
            redactor.redact_text(&format!("ssid PK-{suffix}")),
            "ssid PK-<MAC>"
        );
        let blob = vec![0x00, MAC[3], MAC[4], MAC[5], 0x00, 0x00, 0x00, 0x00];
        assert_eq!(redactor.redact_bytes(&blob), blob);
    }

    #[test]
    fn the_longest_member_at_an_offset_wins() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        // The full MAC contains its own suffix; it must come back as one token, not two.
        let line = redactor.redact_text(&hex(&MAC, Some(b':')));
        assert_eq!(line, "<MAC>");
        assert_eq!(line.matches("<MAC>").count(), 1);
    }

    #[test]
    fn an_address_the_agent_supplied_itself_is_left_alone() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        // Redaction is by value, so a scripted peer address is not masked.
        let scripted = hex(&[0x02, 0x00, 0x00, 0xaa, 0xbb, 0xcc], Some(b':'));
        let line = format!("connect {scripted}");
        assert_eq!(redactor.redact_text(&line), line);
    }

    #[test]
    fn nvs_credentials_cardid_and_backup_stems_become_the_secret_token() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let credential = String::from_utf8(CREDENTIAL.to_vec()).expect("ascii");
        assert_eq!(
            redactor.redact_text(&format!("wifi psk {credential} ok")),
            "wifi psk <SECRET> ok"
        );
        let chunk = String::from_utf8(cardid_chunk()).expect("ascii");
        assert_eq!(redactor.redact_text(&chunk), "<SECRET>");
        assert_eq!(
            redactor.redact_text("restored from passport-backup-fixture.bin"),
            "restored from <SECRET>.bin"
        );
    }

    #[test]
    fn a_credential_in_hex_or_base64_is_redacted() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        assert_eq!(redactor.redact_text(&hex(CREDENTIAL, None)), "<SECRET>");
        let base64 = String::from_utf8(crate::secret_set::encode_base64_prefix(CREDENTIAL))
            .expect("base64 is ASCII");
        assert_eq!(redactor.redact_text(&base64), "<SECRET>");
    }

    #[test]
    fn reveal_identity_keeps_the_mac_and_nothing_else() {
        let set = full_set();
        let redactor = Redactor::with_reveal(&set, Reveal::NONE.with_identity());
        let mac = hex(&MAC, Some(b':'));
        assert_eq!(redactor.redact_text(&mac), mac);
        assert_eq!(redactor.redact_text(&hex(&UID, None)), hex(&UID, None));
        let credential = String::from_utf8(CREDENTIAL.to_vec()).expect("ascii");
        assert_eq!(redactor.redact_text(&credential), "<SECRET>");
        assert!(Reveal::NONE.with_identity().needs_human_confirmation());
    }

    #[test]
    fn reveal_nvs_keeps_the_credential_and_nothing_else() {
        let set = full_set();
        let redactor = Redactor::with_reveal(&set, Reveal::NONE.with_nvs());
        let credential = String::from_utf8(CREDENTIAL.to_vec()).expect("ascii");
        assert_eq!(redactor.redact_text(&credential), credential);
        assert_eq!(redactor.redact_text(&hex(&MAC, Some(b':'))), "<MAC>");
    }

    #[test]
    fn include_secrets_keeps_the_nfc_values_and_nothing_else() {
        let set = full_set();
        let redactor = Redactor::with_reveal(&set, Reveal::NONE.with_include_secrets());
        let uid = hex(&[0x04, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6], Some(b':'));
        assert_eq!(redactor.redact_text(&uid), uid);
        assert_eq!(redactor.redact_text(&hex(&MAC, Some(b':'))), "<MAC>");
    }

    #[test]
    fn the_cardid_is_never_revealed_whatever_the_opt_ins() {
        let set = full_set();
        let every = Reveal::NONE
            .with_identity()
            .with_nvs()
            .with_include_secrets();
        let redactor = Redactor::with_reveal(&set, every);
        let chunk = String::from_utf8(cardid_chunk()).expect("ascii");
        assert_eq!(redactor.redact_text(&chunk), "<SECRET>");
        assert_eq!(
            redactor.redact_text("passport-backup-fixture"),
            "<SECRET>",
            "a backup stem has no opt-in either"
        );
        for kind in [
            MemberKind::CardId,
            MemberKind::BackupStem,
            MemberKind::Canary,
        ] {
            assert!(!every.allows(kind), "{kind} must never be revealed");
        }
    }

    #[test]
    fn a_binary_artifact_keeps_its_length_and_offsets() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let mut blob = b"BTSN\x00\x00\x00\x00".to_vec();
        blob.extend_from_slice(&MAC);
        blob.extend_from_slice(b"\x00trailer");
        let redacted = redactor.redact_bytes(&blob);
        assert_eq!(redacted.len(), blob.len());
        assert_eq!(&redacted[..8], b"BTSN\x00\x00\x00\x00");
        assert_eq!(&redacted[8..14], &[BINARY_FILL; 6]);
        assert_eq!(&redacted[14..], b"\x00trailer");
    }

    #[test]
    fn a_filled_window_can_never_match_again() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let mut blob = MAC.to_vec();
        blob.extend_from_slice(&[0x00, 0x00]);
        let once = redactor.redact_bytes(&blob);
        assert_eq!(redactor.redact_bytes(&once), once);
    }

    #[test]
    fn the_report_counts_by_kind_and_never_names_a_value() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let line = format!("{} and {}", hex(&MAC, Some(b':')), hex(&UID, None));
        let (_, report) = redactor.redact_text_counted(&line);
        assert_eq!(report.replaced, 2);
        assert_eq!(report.by_kind[&MemberKind::Mac], 1);
        assert_eq!(report.by_kind[&MemberKind::UniqueId], 1);
        assert!(!format!("{report:?}").contains(&hex(&MAC, Some(b':'))));
    }

    #[test]
    fn an_empty_set_changes_nothing() {
        let set = SecretSet::builder().build();
        let redactor = Redactor::new(&set);
        assert!(redactor.is_empty());
        assert_eq!(redactor.redact_text("anything at all"), "anything at all");
        assert_eq!(redactor.redact_bytes(b"\x00\x01\x02"), b"\x00\x01\x02");
    }

    #[test]
    fn json_keys_and_values_are_both_redacted() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let mac = hex(&MAC, Some(b':'));
        let mut value = serde_json::json!({
            "namespace": {mac.clone(): "sta"},
            "peers": [format!("peer {mac}")],
            "count": 3,
        });
        let report = redactor.redact_json(&mut value);
        assert_eq!(report.replaced, 2);
        assert_eq!(value["namespace"]["<MAC>"], "sta");
        assert_eq!(value["peers"][0], "peer <MAC>");
        assert_eq!(value["count"], 3);
    }

    /// `redact_json` rewrites JSON strings only, so a secret serialized as an array of numbers
    /// passes through. That is why secret inputs are excluded from journal exports structurally
    /// (`pemu_wasm::instance` `secret_payload`).
    #[test]
    fn secret_bytes_are_invisible_to_the_value_masker() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let bytes: Vec<u8> = CREDENTIAL.to_vec();
        let mut value = serde_json::json!({
            "as_text": String::from_utf8(bytes.clone()).expect("the fixture is ASCII"),
            "as_bytes": bytes,
        });
        let report = redactor.redact_json(&mut value);
        assert_eq!(report.replaced, 1, "only the string form was found");
        assert_eq!(value["as_text"], "<SECRET>");
        assert_eq!(
            value["as_bytes"],
            serde_json::json!(CREDENTIAL.to_vec()),
            "a number array is invisible to the masker by construction"
        );
        assert_ne!(redactor.redact_bytes(CREDENTIAL), CREDENTIAL);
    }

    #[test]
    fn a_secret_that_straddles_the_line_cap_is_masked_before_the_cap_cuts_it() {
        use crate::shape::{LINE_CHARS, ShapeLimits};

        let set = full_set();
        let redactor = Redactor::new(&set);
        let mac = hex(&MAC, Some(b':'));
        // Shaping first would keep 16 of the MAC's 17 characters, which no longer match.
        let filler = "p".repeat(LINE_CHARS - 16);
        let shaped = redactor.shape_text(&format!("{filler}{mac}\n"), &ShapeLimits::DEFAULT);
        assert_eq!(shaped.text(), format!("{filler}{MAC_TOKEN}"));
        assert!(!shaped.text().contains(&mac[..mac.len() - 1]));
        assert_eq!(shaped.head[0].cut_chars, 0, "the masked line fits the cap");
    }

    #[test]
    fn a_serial_excerpt_is_redacted_without_moving_the_cursor() {
        use crate::shape::{Cursor, LINE_CHARS, ShapeLimits, shape_serial};

        let set = full_set();
        let redactor = Redactor::new(&set);
        let mac = hex(&MAC, Some(b':'));
        let chunk = format!("{}{mac}\npartial", "q".repeat(LINE_CHARS - 16));
        let excerpt =
            redactor.shape_serial(chunk.as_bytes(), Cursor(64), &[], &ShapeLimits::DEFAULT);
        assert!(excerpt.to_text().ends_with(MAC_TOKEN));
        assert!(!excerpt.to_text().contains(&mac[..mac.len() - 1]));
        // A cursor names a channel byte offset, so it is the unredacted one.
        let plain = shape_serial(chunk.as_bytes(), Cursor(64), &[], &ShapeLimits::DEFAULT);
        assert_eq!(excerpt.cursor, plain.cursor);
        assert_eq!(excerpt.next_cursor, plain.next_cursor);
        assert_eq!(excerpt.bytes, plain.bytes);
        assert_eq!(excerpt.held_bytes, plain.held_bytes);
    }

    #[test]
    fn two_sibling_keys_that_redact_to_one_token_both_survive() {
        // Several keys redact to `<MAC>`, and redaction must not drop a row.
        let set = full_set();
        let redactor = Redactor::new(&set);
        let mut derived = MAC;
        derived[5] = derived[5].wrapping_add(1);
        let mut value = serde_json::json!({
            hex(&MAC, Some(b':')): "station",
            hex(&derived, Some(b':')): "softap",
            "keep": 1,
        });
        let report = redactor.redact_json(&mut value);
        assert_eq!(report.replaced, 2);
        let map = value.as_object().expect("an object");
        assert_eq!(map.len(), 3, "no entry was dropped: {map:?}");
        let masked: Vec<&serde_json::Value> = map
            .iter()
            .filter(|(key, _)| key.starts_with(MAC_TOKEN))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(masked.len(), 2);
        assert!(masked.contains(&&serde_json::json!("station")));
        assert!(masked.contains(&&serde_json::json!("softap")));
        assert_eq!(map["keep"], 1);
    }

    #[test]
    fn a_redacted_artifact_path_is_still_a_valid_artifact_path() {
        use crate::output::{ArtifactRef, check_artifact_path};

        let set = full_set();
        let redactor = Redactor::new(&set);
        // A tainted machine's instance directory can be named after its own MAC.
        let bare = hex(&MAC, None);
        let mut output = Output::new(serde_json::json!({}), "", Receipt::default())
            .with_artifact(
                ArtifactRef::new(
                    format!("{bare}/p1/screen.png"),
                    "b".repeat(64),
                    "image/png",
                    9,
                )
                .expect("a relative forward-slashed path"),
            )
            .expect("a valid artifact path");
        let report = redactor.redact_output(&mut output);
        assert_eq!(report.replaced, 1);
        let path = &output.artifacts[0].path;
        assert_eq!(path, &format!("{PATH_TOKEN}/p1/screen.png"));
        assert!(!path.contains(&bare));
        assert_eq!(check_artifact_path(path), Ok(()), "{path}");
    }

    #[test]
    fn a_whole_output_is_redacted_text_json_and_receipt() {
        let set = full_set();
        let redactor = Redactor::new(&set);
        let mac = hex(&MAC, Some(b':'));
        let mut receipt = Receipt::default();
        receipt
            .extra
            .insert("bound_peer".to_string(), mac.clone().into());
        let mut output = Output::new(
            serde_json::json!({"sta_mac": mac.clone()}),
            format!("I (412) wifi: sta mac {mac}"),
            receipt,
        );
        let report = redactor.redact_output(&mut output);
        assert_eq!(report.replaced, 3);
        assert_eq!(output.json["sta_mac"], "<MAC>");
        assert_eq!(output.text, "I (412) wifi: sta mac <MAC>");
        assert_eq!(output.receipt.extra["bound_peer"], "<MAC>");
        assert!(!output.to_text().contains(&mac));
    }

    #[test]
    fn an_error_envelope_is_redacted_including_its_serial_tail() {
        use crate::error::E_TIMEOUT;

        let set = full_set();
        let redactor = Redactor::new(&set);
        let mac = hex(&MAC, Some(b':'));
        let mut error = ApiError::new(E_TIMEOUT, format!("no match after {mac} appeared"))
            .with_hint(format!("call status; the peer was {mac}"))
            .with_detail(serde_json::json!({"peer": mac.clone()}))
            .with_serial_tail(vec![
                format!("I (412) wifi: sta mac {mac}"),
                "I (413) pk_app: ready".to_string(),
            ]);
        let report = redactor.redact_error(&mut error);
        assert_eq!(report.replaced, 4);
        assert_eq!(error.message, "no match after <MAC> appeared");
        assert_eq!(
            error.hint.as_deref(),
            Some("call status; the peer was <MAC>")
        );
        assert_eq!(error.detail["peer"], "<MAC>");
        assert_eq!(error.serial_tail[0], "I (412) wifi: sta mac <MAC>");
        assert_eq!(error.serial_tail[1], "I (413) pk_app: ready");
        assert!(!error.to_json_text().contains(&mac));
    }

    #[test]
    fn the_redacted_label_is_a_salted_hash_and_the_window_is_erased() {
        let salt = b"a-sixteen-byte-salt!";
        let mut window = cardid_chunk();
        let label = redact_cardid_window(&mut window, salt);
        assert!(label.starts_with("Redacted{"));
        assert!(label.ends_with('}'));
        assert_eq!(label.len(), "Redacted{}".len() + 64);
        assert!(window.iter().all(|&b| b == 0xFF));
        let mut again = cardid_chunk();
        assert_eq!(redact_cardid_window(&mut again, salt), label);
        let chunk = String::from_utf8(cardid_chunk()).expect("ascii");
        assert!(!label.contains(&chunk));
    }
}
