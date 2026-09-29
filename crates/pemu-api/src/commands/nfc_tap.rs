//! `passportsim nfc_tap`: a virtual phone taps the NTAG213 on the board. This file also holds the
//! card half `nfc_tag` shares.
//!
//! A tap is one journaled `InputEvent::NfcTap { ops: [FieldOn, Cmd(frame)..., FieldOff] }`. The
//! card has no MCU connection, so the reader runs here over a copy of the card from the
//! `board.world` snapshot section: run the ops against the copy, journal the frames that ran, then
//! read the card back, which must equal the copy (else `E_INTERNAL`). No instruction runs, so
//! virtual time does not advance.
//!
//! `dwell_ms` bounds the ops at [`ANTICOLLISION_MS`] plus [`FRAME_MS`] per frame; a frame that
//! would end late is not sent, its op reports `removed` and later ops `not_run`. Both costs are
//! class C estimates for ISO/IEC 14443-A at 106 kbit/s, not measurements; a WRITE is atomic.
//!
//! Redaction: every output is redacted with the instance's set plus the card's UID, PWD and PACK
//! (`card_secrets`). A PWD or PACK the caller supplies and a Wi-Fi key in an NDEF record are
//! secret inputs that taint the instance ([`UserSecrets::from_sources`]), forks included. Byte
//! dumps are masked before rendering (`secret_mask`), so a key split across pages is masked too.

use std::fmt::Write as _;

use pemu_board::ntag213::{
    BoardWorld, CardResponse, Ntag213, PAGE_BYTES, PAGE_CFG1, PAGE_MAX, PAGE_PACK, PAGE_PWD,
    USER_FIRST, USER_LAST, cmd,
    ndef::{self, NdefRecord},
};
use pemu_core::input::{InputEvent, NfcOp};
use pemu_core::snap::{SectionId, SnapOpts, serde_from_section};
use pemu_machine::machine::At;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_SNAPSHOT, E_STATE, E_USAGE};
use crate::instance::InstanceId;
use crate::output::Output;
use crate::redact::Redactor;
use crate::registry::command;
use crate::secret_set::{SecretSet, SecretSetBuilder};
use crate::shape::ShapeLimits;
use crate::spec::{Annotations, HandlerCx, Schema};

use crate::args::{instance_schema, object, only, opt_str, req_str, usage};
use crate::pool::Pool;
use crate::session::Session;

pub const SECRET: &str = "<SECRET>";
pub const DEFAULT_DWELL_MS: u64 = 300;
pub const DWELL_MS_MAX: u64 = 60_000;
/// Field-on and anticollision. Class C, see the module docs.
pub const ANTICOLLISION_MS: u64 = 10;
/// One command frame and its answer. Class C, see the module docs.
pub const FRAME_MS: u64 = 5;
/// So a call cannot journal an unbounded input.
pub const OPS_MAX: usize = 64;
pub const FRAMES_MAX: usize = 64;
/// MIME type of the Wi-Fi Simple Configuration record.
pub const WSC_MIME: &str = "application/vnd.wfa.wsc";

/// WSC attribute ids.
mod wsc {
    pub const VERSION1: u16 = 0x104A;
    pub const CREDENTIAL: u16 = 0x100E;
    pub const NETWORK_INDEX: u16 = 0x1026;
    pub const SSID: u16 = 0x1045;
    pub const AUTH_TYPE: u16 = 0x1003;
    pub const ENCR_TYPE: u16 = 0x100F;
    pub const NETWORK_KEY: u16 = 0x1027;
    pub const MAC_ADDRESS: u16 = 0x1020;
    pub const VENDOR_EXT: u16 = 0x1049;
}

/// The accepted `auth` names and their WSC AuthenticationType codes. `wpa` and `wpa2` are the short
/// forms the web panel sends.
const AUTH_TYPES: [(&str, u16); 5] = [
    ("open", 0x0001),
    ("wpa-personal", 0x0002),
    ("wpa", 0x0002),
    ("wpa2-personal", 0x0020),
    ("wpa2", 0x0020),
];
/// The accepted `encr` names and their WSC EncryptionType codes.
const ENCR_TYPES: [(&str, u16); 4] = [
    ("none", 0x0001),
    ("wep", 0x0002),
    ("tkip", 0x0004),
    ("aes", 0x0008),
];

/// 16-bit type, 16-bit length, value, big-endian.
fn wsc_attr(out: &mut Vec<u8>, id: u16, value: &[u8]) {
    out.extend_from_slice(&id.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
}

/// Version1, a Credential (NetworkIndex, SSID, auth, encryption, key, broadcast MacAddress), and
/// the WFA vendor extension carrying Version2. The attribute order and the MacAddress value phones
/// accept are unverified.
pub fn wifi_payload(ssid: &str, auth: u16, encr: u16, key: &str) -> Vec<u8> {
    let mut credential = Vec::new();
    wsc_attr(&mut credential, wsc::NETWORK_INDEX, &[0x01]);
    wsc_attr(&mut credential, wsc::SSID, ssid.as_bytes());
    wsc_attr(&mut credential, wsc::AUTH_TYPE, &auth.to_be_bytes());
    wsc_attr(&mut credential, wsc::ENCR_TYPE, &encr.to_be_bytes());
    wsc_attr(&mut credential, wsc::NETWORK_KEY, key.as_bytes());
    wsc_attr(&mut credential, wsc::MAC_ADDRESS, &[0xFF; 6]);
    let mut payload = Vec::new();
    wsc_attr(&mut payload, wsc::VERSION1, &[0x10]);
    wsc_attr(&mut payload, wsc::CREDENTIAL, &credential);
    wsc_attr(
        &mut payload,
        wsc::VENDOR_EXT,
        &[0x00, 0x37, 0x2A, 0x00, 0x01, 0x20],
    );
    payload
}

fn wsc_attrs(mut bytes: &[u8]) -> Option<Vec<(u16, &[u8])>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let id = u16::from_be_bytes(bytes.get(0..2)?.try_into().ok()?);
        let len = u16::from_be_bytes(bytes.get(2..4)?.try_into().ok()?) as usize;
        out.push((id, bytes.get(4..4 + len)?));
        bytes = &bytes[4 + len..];
    }
    Some(out)
}

/// Of the first Credential, if there is one.
fn wifi_of(payload: &[u8]) -> Option<(String, String, String)> {
    let credential = wsc_attrs(payload)?
        .into_iter()
        .find(|(id, _)| *id == wsc::CREDENTIAL)?
        .1;
    let attrs = wsc_attrs(credential)?;
    let find = |want: u16| attrs.iter().find(|(id, _)| *id == want).map(|(_, v)| *v);
    let ssid = String::from_utf8(find(wsc::SSID)?.to_vec()).ok()?;
    let code = |value: Option<&[u8]>| {
        value
            .and_then(|v| v.try_into().ok())
            .map(u16::from_be_bytes)
    };
    let name = |table: &[(&'static str, u16)], value: Option<u16>| {
        value
            .and_then(|v| {
                table
                    .iter()
                    .find(|(_, c)| *c == v)
                    .map(|(n, _)| (*n).to_owned())
            })
            .unwrap_or_else(|| format!("0x{:04x}", value.unwrap_or(0)))
    };
    // The long names are canonical, so the short aliases are skipped when reporting.
    let auth_table: Vec<(&str, u16)> = AUTH_TYPES
        .iter()
        .copied()
        .filter(|(n, _)| *n != "wpa" && *n != "wpa2")
        .collect();
    Some((
        ssid,
        name(&auth_table, code(find(wsc::AUTH_TYPE))),
        name(&ENCR_TYPES, code(find(wsc::ENCR_TYPE))),
    ))
}

fn wifi_key(payload: &[u8]) -> Option<Vec<u8>> {
    let credential = wsc_attrs(payload)?
        .into_iter()
        .find(|(id, _)| *id == wsc::CREDENTIAL)?
        .1;
    wsc_attrs(credential)?
        .into_iter()
        .find(|(id, value)| *id == wsc::NETWORK_KEY && !value.is_empty())
        .map(|(_, value)| value.to_vec())
}

/// `uri`, `text` or `wifi`, as the web contract `NdefRecord` spells them.
fn record_from_json(value: &serde_json::Value, at: &str) -> Result<NdefRecord, ApiError> {
    let args = value
        .as_object()
        .ok_or_else(|| usage(at, "expected a record object with a `type`"))?;
    match req_str(args, "type").map_err(|_| usage(at, "`type` is required"))? {
        "uri" => {
            only(args, &["type", "uri"])?;
            Ok(NdefRecord::Uri(req_str(args, "uri")?.to_owned()))
        }
        "text" => {
            only(args, &["type", "text", "lang"])?;
            let lang = opt_str(args, "lang")?.unwrap_or("en");
            if lang.is_empty() || lang.len() > 0x3F {
                return Err(usage(&format!("{at}.lang"), "expected 1 to 63 bytes"));
            }
            Ok(NdefRecord::Text {
                lang: lang.to_owned(),
                text: req_str(args, "text")?.to_owned(),
            })
        }
        "wifi" => {
            only(args, &["type", "ssid", "auth", "encr", "key"])?;
            let ssid = req_str(args, "ssid")?;
            if ssid.is_empty() || ssid.len() > 32 {
                return Err(usage(&format!("{at}.ssid"), "expected 1 to 32 bytes"));
            }
            let pick = |key: &str, table: &[(&str, u16)], default: &str| {
                let text = opt_str(args, key)?.unwrap_or(default);
                table
                    .iter()
                    .find(|(name, _)| *name == text)
                    .map(|(_, code)| *code)
                    .ok_or_else(|| {
                        let names: Vec<&str> = table.iter().map(|(n, _)| *n).collect();
                        usage(
                            &format!("{at}.{key}"),
                            &format!("`{text}` is not one of {names:?}"),
                        )
                    })
            };
            let auth = pick("auth", &AUTH_TYPES, "wpa2-personal")?;
            let encr = pick(
                "encr",
                &ENCR_TYPES,
                if auth == 0x0001 { "none" } else { "aes" },
            )?;
            let key = opt_str(args, "key")?.unwrap_or("");
            if key.len() > 64 {
                return Err(usage(&format!("{at}.key"), "expected at most 64 bytes"));
            }
            // A key the set cannot hold (under `MIN_CREDENTIAL_LEN` bytes, or one repeated byte)
            // could not be masked or taint the instance, so it is refused rather than written in
            // the clear.
            let mut trackable = SecretSetBuilder::new();
            trackable.nvs_credential(key.as_bytes());
            if !key.is_empty() && trackable.build().is_empty() {
                return Err(usage(
                    &format!("{at}.key"),
                    "a key must be empty or at least 6 bytes and not one repeated byte",
                ));
            }
            Ok(NdefRecord::Mime {
                mime_type: WSC_MIME.to_owned(),
                payload: wifi_payload(ssid, auth, encr, key),
            })
        }
        other => Err(usage(
            &format!("{at}.type"),
            &format!("`{other}` is not one of uri, text, wifi"),
        )),
    }
}

pub(crate) fn records_from_json(
    value: &serde_json::Value,
    at: &str,
) -> Result<Vec<NdefRecord>, ApiError> {
    let items = value
        .as_array()
        .ok_or_else(|| usage(at, "expected an array of records"))?;
    if items.is_empty() {
        return Err(usage(at, "expected at least one record"));
    }
    items
        .iter()
        .enumerate()
        .map(|(i, item)| record_from_json(item, &format!("{at}[{i}]")))
        .collect()
}

/// A Wi-Fi key is written as `<SECRET>`: it is a credential, and the agent that wrote it has it.
pub(crate) fn record_json(record: &NdefRecord) -> serde_json::Value {
    match record {
        NdefRecord::Uri(uri) => serde_json::json!({ "type": "uri", "uri": uri }),
        NdefRecord::Text { lang, text } => {
            serde_json::json!({ "type": "text", "text": text, "lang": lang })
        }
        NdefRecord::Mime { mime_type, payload } if mime_type == WSC_MIME => {
            match wifi_of(payload) {
                Some((ssid, auth, encr)) => serde_json::json!({
                    "type": "wifi", "ssid": ssid, "auth": auth, "encr": encr, "key": SECRET
                }),
                None => {
                    serde_json::json!({ "type": "mime", "mime_type": mime_type, "payload_hex": hex(payload) })
                }
            }
        }
        NdefRecord::Mime { mime_type, payload } => {
            serde_json::json!({ "type": "mime", "mime_type": mime_type, "payload_hex": hex(payload) })
        }
        NdefRecord::Other {
            tnf,
            type_bytes,
            payload,
        } => serde_json::json!({
            "type": "raw", "tnf": tnf, "type_hex": hex(type_bytes), "payload_hex": hex(payload)
        }),
    }
}

/// Upper-case hex without separators.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

/// Spaces are allowed between bytes.
fn parse_hex(text: &str, at: &str) -> Result<Vec<u8>, ApiError> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return Err(usage(at, "expected a non-empty even number of hex digits"));
    }
    digits
        .chunks(2)
        .map(|pair| {
            core::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or_else(|| usage(at, "expected hex digits"))
        })
        .collect()
}

/// The pending journal is applied first (a zero-length run), then the `board.world` section of a
/// snapshot is decoded.
pub(crate) fn read_card(session: &mut Session) -> Result<Ntag213, ApiError> {
    let now = session.now();
    session.run_until(now);
    let snapshot = session
        .snapshot_machine()
        .snapshot(SnapOpts::default())
        .map_err(|e| {
            ApiError::new(
                E_SNAPSHOT,
                format!("the card cannot be read out of this machine: {e:?}"),
            )
            .with_hint("the NFC commands need a backend that can take a snapshot")
        })?;
    let id = SectionId::board("world");
    let section = snapshot
        .section(&id)
        .map_err(|e| ApiError::new(E_SNAPSHOT, format!("no board.world section: {e:?}")))?;
    let world: BoardWorld = serde_from_section(
        section,
        id,
        pemu_machine::snapshot::SECTION_VERSION,
        "board.world",
    )
    .map_err(|e| ApiError::new(E_SNAPSHOT, format!("board.world does not decode: {e:?}")))?;
    Ok(world.card)
}

/// Journals the frames that ran as one tap, then checks the machine's card against the copy they
/// ran on.
pub(crate) fn journal_tap(
    session: &mut Session,
    frames: &[Vec<u8>],
    expected: &Ntag213,
) -> Result<(), ApiError> {
    let mut ops = Vec::with_capacity(frames.len() + 2);
    ops.push(NfcOp::FieldOn);
    ops.extend(frames.iter().cloned().map(NfcOp::Cmd));
    ops.push(NfcOp::FieldOff);
    session
        .machine()
        .input(At::Now, InputEvent::NfcTap { ops })
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the tap at the current instant",
            )
        })?;
    let after = read_card(session)?;
    if after != *expected {
        return Err(ApiError::new(
            E_INTERNAL,
            "the journaled tap left the card different from the reader's copy",
        ));
    }
    Ok(())
}

/// The instance's set plus the card's UID, PWD and PACK, for this output only. The store's set is
/// not replaced: a card whose UID the seed synthesized is not a secret input. `extra` reaches the
/// store at its next extension.
pub(crate) fn card_secrets(
    session: &Session,
    card: &Ntag213,
    extra: &UserSecrets,
) -> crate::secret_set::SecretSet {
    let id = session.id;
    let mut builder =
        SecretSetBuilder::from_set(session.with_store(|store| store.secret_set(id).clone()));
    extra.add_to(&mut builder);
    builder.nfc_uid(&card.uid());
    if let Some(pwd) = card.page(PAGE_PWD) {
        builder.nfc_pwd(&pwd);
    }
    if let Some(pack) = card.page(PAGE_PACK) {
        builder.nfc_pack(&pack[0..2]);
    }
    builder.build()
}

pub(crate) fn redacted(
    session: &Session,
    card: &Ntag213,
    extra: &UserSecrets,
    mut output: Output,
) -> Output {
    let set = card_secrets(session, card, extra);
    Redactor::new(&set).redact_output(&mut output);
    output
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserSecrets {
    pwds: Vec<Vec<u8>>,
    packs: Vec<Vec<u8>>,
    keys: Vec<Vec<u8>>,
}

impl UserSecrets {
    /// PWD and PACK from a WRITE or COMP_WRITE of page 0x2B or 0x2C, and the password of a
    /// PWD_AUTH.
    pub(crate) fn add_frames(&mut self, frames: &[Vec<u8>]) {
        for frame in frames {
            let write = match frame.as_slice() {
                [cmd::WRITE, page, data @ ..] if data.len() == PAGE_BYTES => Some((*page, data)),
                [cmd::COMP_WRITE, page, data @ ..] if data.len() == 16 => {
                    Some((*page, &data[..PAGE_BYTES]))
                }
                [cmd::PWD_AUTH, pwd @ ..] if pwd.len() == PAGE_BYTES => {
                    self.pwds.push(pwd.to_vec());
                    None
                }
                _ => None,
            };
            match write {
                Some((PAGE_PWD, data)) => self.pwds.push(data.to_vec()),
                Some((PAGE_PACK, data)) => self.packs.push(data[..2].to_vec()),
                _ => {}
            }
        }
    }

    pub(crate) fn add_records(&mut self, records: &[NdefRecord]) {
        for record in records {
            if let NdefRecord::Mime { mime_type, payload } = record
                && mime_type == WSC_MIME
                && let Some(key) = wifi_key(payload)
            {
                self.keys.push(key);
            }
        }
    }

    /// Also catches a record a `raw` op wrote page by page.
    pub(crate) fn add_card(&mut self, card: &Ntag213) {
        if let Ok(records) = card.ndef_read() {
            self.add_records(&records);
        }
    }

    pub(crate) fn merge(&mut self, other: &UserSecrets) {
        self.pwds.extend(other.pwds.iter().cloned());
        self.packs.extend(other.packs.iter().cloned());
        self.keys.extend(other.keys.iter().cloned());
    }

    pub(crate) fn lacks_any_of(&self, other: &UserSecrets) -> bool {
        other.pwds.iter().any(|v| !self.pwds.contains(v))
            || other.packs.iter().any(|v| !self.packs.contains(v))
            || other.keys.iter().any(|v| !self.keys.contains(v))
    }

    /// The card's PWD and PACK, the Wi-Fi keys in its NDEF message, and the PWD and PACK of the
    /// journaled tap frames. The delivery PWD (all ones) and PACK (zero) are uniform, so
    /// [`SecretSetBuilder`] drops them and a seed card yields no member.
    pub fn from_sources(sources: &pemu_machine::snapshot::SecretSources) -> UserSecrets {
        let mut out = UserSecrets::default();
        let page = |p: u8| {
            let at = p as usize * PAGE_BYTES;
            sources.nfc_card.get(at..at + PAGE_BYTES)
        };
        if let Some(pwd) = page(PAGE_PWD) {
            out.pwds.push(pwd.to_vec());
        }
        if let Some(pack) = page(PAGE_PACK) {
            out.packs.push(pack[..2].to_vec());
        }
        let user = sources
            .nfc_card
            .get(USER_FIRST as usize * PAGE_BYTES..(USER_LAST as usize + 1) * PAGE_BYTES);
        if let Some(area) = user
            && let Ok(tlv) = ndef::find_ndef(area)
            && let Some(message) = area.get(tlv.value..tlv.value + tlv.len)
            && let Ok(records) = ndef::decode_message(message)
        {
            out.add_records(&records);
        }
        out.add_frames(&sources.nfc_frames);
        out
    }

    pub(crate) fn add_to(&self, builder: &mut SecretSetBuilder) {
        for pwd in &self.pwds {
            builder.nfc_pwd(pwd);
        }
        for pack in &self.packs {
            builder.nfc_pack(pack);
        }
        for key in &self.keys {
            builder.nvs_credential(key);
        }
    }

    /// A member a set keeps is what makes an input secret-bearing.
    pub fn is_secret(&self) -> bool {
        let mut builder = SecretSetBuilder::new();
        self.add_to(&mut builder);
        !builder.build().is_empty()
    }
}

/// Every member not limited to text.
pub(crate) fn secret_values(set: &SecretSet) -> Vec<Vec<u8>> {
    set.members()
        .iter()
        .filter(|member| !member.text_only)
        .map(|member| member.bytes.clone())
        .collect()
}

fn memory(card: &Ntag213) -> Vec<u8> {
    (0..=PAGE_MAX)
        .flat_map(|page| card.page(page).unwrap_or([0; PAGE_BYTES]))
        .collect()
}

/// UID, BCC1, PWD or PACK.
fn positional_secret(page: u8, byte: usize) -> bool {
    match page {
        0 | 1 | PAGE_PWD => true,
        2 => byte == 0,
        PAGE_PACK => byte < 2,
        _ => false,
    }
}

fn value_mask(data: &[u8], values: &[Vec<u8>]) -> Vec<bool> {
    let mut mask = vec![false; data.len()];
    for value in values.iter().filter(|v| !v.is_empty()) {
        for at in 0..data.len().saturating_sub(value.len() - 1) {
            if data[at..at + value.len()] == value[..] {
                mask[at..at + value.len()].fill(true);
            }
        }
    }
    mask
}

/// One flag per byte, page 0 first: the positional ones and every occurrence of `values`.
pub(crate) fn secret_mask(card: &Ntag213, values: &[Vec<u8>]) -> Vec<bool> {
    let mut mask = value_mask(&memory(card), values);
    for (i, flag) in mask.iter_mut().enumerate() {
        *flag |= positional_secret((i / PAGE_BYTES) as u8, i % PAGE_BYTES);
    }
    mask
}

/// Each run of masked bytes becomes one `<SECRET>`.
pub(crate) fn masked_hex(bytes: &[u8], mask: &[bool]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    let mut masked = false;
    for (i, byte) in bytes.iter().enumerate() {
        let secret = mask.get(i).copied().unwrap_or(false);
        if secret && !masked {
            out.push_str(SECRET);
        } else if !secret {
            let _ = write!(out, "{byte:02X}");
        }
        masked = secret;
    }
    out
}

/// As `env` does.
pub(crate) fn bind_checked(
    pool: &mut Pool,
    annotations: Annotations,
    instance: Option<&str>,
) -> Result<InstanceId, ApiError> {
    let id = pool.bind(annotations, instance)?;
    let now = pool
        .session(id)
        .map(Session::now)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    if let Some(state) = pool.table().get(id) {
        state
            .lease
            .check_call(crate::lease::LeaseHolder::Agent, annotations, now)?;
    }
    Ok(id)
}

pub(crate) fn locked_pages(card: &Ntag213) -> Vec<u8> {
    (0..=PAGE_MAX).filter(|p| card.is_locked(*p)).collect()
}

/// Pages 0x04 to 0x27.
fn user_area(card: &Ntag213) -> Vec<u8> {
    (USER_FIRST..=USER_LAST)
        .flat_map(|p| card.page(p).unwrap_or([0; PAGE_BYTES]))
        .collect()
}

/// Never the UID, PWD or PACK.
pub(crate) fn tag_json(card: &Ntag213) -> serde_json::Value {
    let area = user_area(card);
    let (used, records) = match ndef::find_ndef(&area) {
        Ok(tlv) => {
            let records = match card.ndef_read() {
                Ok(records) => serde_json::Value::Array(records.iter().map(record_json).collect()),
                Err(e) => serde_json::json!({ "error": format!("{e:?}") }),
            };
            ((tlv.value + tlv.len + 1).min(area.len()), records)
        }
        Err(e) => (0, serde_json::json!({ "error": format!("{e:?}") })),
    };
    serde_json::json!({
        "type": "NTAG213",
        "uid": SECRET,
        "pages_total": PAGE_MAX as usize + 1,
        "user_bytes": area.len(),
        "cc_hex": hex(&card.page(3).unwrap_or_default()),
        "locked_pages": locked_pages(card),
        "ndef_bytes_used": used,
        "ndef_bytes_free": area.len() - used,
        "counter": card.counter(),
        "counter_enabled": card.counter_enabled(),
        "records": records,
    })
}

pub(crate) fn records_text(records: &serde_json::Value) -> String {
    match records.as_array() {
        Some(list) if list.is_empty() => "no records".to_owned(),
        Some(list) => list
            .iter()
            .map(|r| match r["type"].as_str() {
                Some("uri") => format!("uri {}", r["uri"].as_str().unwrap_or("")),
                Some("text") => format!("text {:?}", r["text"].as_str().unwrap_or("")),
                Some("wifi") => format!("wifi {}", r["ssid"].as_str().unwrap_or("")),
                Some(other) => other.to_owned(),
                None => "?".to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", "),
        None => format!(
            "records unreadable: {}",
            records["error"].as_str().unwrap_or("?")
        ),
    }
}

/// One WRITE per changed page, in page order, turning `card`'s user pages into what
/// `Ntag213::ndef_write` would make of `records`.
pub(crate) fn ndef_write_frames(
    card: &Ntag213,
    records: &[NdefRecord],
    at: &str,
) -> Result<Vec<Vec<u8>>, ApiError> {
    let mut target = card.clone();
    target
        .ndef_write(records)
        .map_err(|e| usage(at, &format!("the message does not fit the tag: {e:?}")))?;
    Ok((USER_FIRST..=USER_LAST)
        .filter_map(|page| {
            let new = target.page(page)?;
            (card.page(page)? != new).then(|| {
                let mut frame = vec![cmd::WRITE, page];
                frame.extend_from_slice(&new);
                frame
            })
        })
        .collect())
}

pub(crate) fn write_frame(page: u8, data: [u8; PAGE_BYTES]) -> Vec<u8> {
    let mut frame = vec![cmd::WRITE, page];
    frame.extend_from_slice(&data);
    frame
}

/// Sets NFC_CNT_EN, keeping the rest of CFG1.
pub(crate) fn counter_enable_frame(card: &Ntag213) -> Vec<u8> {
    let mut cfg1 = card.page(PAGE_CFG1).unwrap_or_default();
    cfg1[0] |= 0x10;
    write_frame(PAGE_CFG1, cfg1)
}

/// As the raw console prints it (ACK `0A`, NAK its code), with secret bytes as `<SECRET>`.
///
/// A READ or FAST_READ answer is masked byte by byte with the card's [`secret_mask`] plus any
/// occurrence of `values` in the answer (a READ that rolls over to page 0): redaction looks for
/// whole byte strings, which a dump that splits a UID around BCC0 or a key across pages never has.
/// PWD_AUTH answers with PACK itself, so its answer is masked whole.
fn response_hex(
    frame: &[u8],
    response: &CardResponse,
    card_mask: &[bool],
    values: &[Vec<u8>],
) -> String {
    let CardResponse::Data(data) = response else {
        return hex(&response.to_bytes());
    };
    let pages: Vec<u8> = match frame {
        [cmd::READ, addr] => (0..4)
            .map(|step| addr.wrapping_add(step) % (PAGE_MAX + 1))
            .collect(),
        [cmd::FAST_READ, start, end] => (*start..=*end).collect(),
        [cmd::PWD_AUTH, ..] => return SECRET.to_owned(),
        _ => return hex(data),
    };
    let mut mask = value_mask(data, values);
    for (k, flag) in mask.iter_mut().enumerate() {
        let page = pages.get(k / PAGE_BYTES).copied().unwrap_or(0) as usize;
        *flag |= card_mask
            .get(page * PAGE_BYTES + k % PAGE_BYTES)
            .copied()
            .unwrap_or(false);
    }
    masked_hex(data, &mask)
}

/// Web contract `NfcOp`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TapOp {
    /// FAST_READ of the user pages and the NDEF records in them.
    ReadNdef,
    /// Replace the NDEF message, as WRITE commands of the pages it changes.
    WriteNdef(Vec<NdefRecord>),
    /// No CRC_A.
    Raw(Vec<Vec<u8>>),
}

impl TapOp {
    fn name(&self) -> &'static str {
        match self {
            TapOp::ReadNdef => "readNdef",
            TapOp::WriteNdef(_) => "writeNdef",
            TapOp::Raw(_) => "raw",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NfcTapArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub ops: Vec<TapOp>,
    /// In virtual milliseconds.
    pub dwell_ms: u64,
}

impl NfcTapArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<NfcTapArgs, ApiError> {
        let args = object(value)?;
        only(args, &["instance", "ops", "dwell_ms"])?;
        let items = args
            .get("ops")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| usage("ops", "expected an array of ops"))?;
        if items.is_empty() || items.len() > OPS_MAX {
            return Err(usage("ops", &format!("expected 1 to {OPS_MAX} ops")));
        }
        let mut ops = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let at = format!("ops[{i}]");
            let op = item
                .as_object()
                .ok_or_else(|| usage(&at, "expected an op object"))?;
            ops.push(match req_str(op, "op")? {
                "readNdef" => {
                    only(op, &["op"])?;
                    TapOp::ReadNdef
                }
                "writeNdef" => {
                    only(op, &["op", "ndef"])?;
                    let ndef = op
                        .get("ndef")
                        .ok_or_else(|| usage(&at, "`ndef` is required"))?;
                    TapOp::WriteNdef(records_from_json(ndef, &format!("{at}.ndef"))?)
                }
                "raw" => {
                    only(op, &["op", "frames"])?;
                    let frames = op
                        .get("frames")
                        .and_then(serde_json::Value::as_array)
                        .ok_or_else(|| usage(&at, "`frames` is required"))?;
                    if frames.is_empty() || frames.len() > FRAMES_MAX {
                        return Err(usage(&at, &format!("expected 1 to {FRAMES_MAX} frames")));
                    }
                    TapOp::Raw(
                        frames
                            .iter()
                            .enumerate()
                            .map(|(j, f)| {
                                let fat = format!("{at}.frames[{j}]");
                                f.as_str()
                                    .ok_or_else(|| usage(&fat, "expected a hex string"))
                                    .and_then(|s| parse_hex(s, &fat))
                            })
                            .collect::<Result<_, _>>()?,
                    )
                }
                other => {
                    return Err(usage(
                        &format!("{at}.op"),
                        &format!("`{other}` is not one of readNdef, writeNdef, raw"),
                    ));
                }
            });
        }
        let dwell_ms = match args.get("dwell_ms") {
            None | Some(serde_json::Value::Null) => DEFAULT_DWELL_MS,
            Some(v) => v.as_u64().filter(|ms| *ms <= DWELL_MS_MAX).ok_or_else(|| {
                usage(
                    "dwell_ms",
                    &format!("expected an integer in 0..={DWELL_MS_MAX}"),
                )
            })?,
        };
        Ok(NfcTapArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            ops,
            dwell_ms,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TapRun {
    /// Field off.
    pub card: Ntag213,
    pub frames: Vec<Vec<u8>>,
    pub ops: Vec<serde_json::Value>,
    /// Anticollision included.
    pub elapsed_ms: u64,
    pub removed: bool,
}

/// The reader side of a tap, masking `values` in the answers it reports.
pub fn run_tap(mut card: Ntag213, ops: &[TapOp], dwell_ms: u64, values: &[Vec<u8>]) -> TapRun {
    let mut run = TapRun {
        card: Ntag213::default(),
        frames: Vec::new(),
        ops: Vec::new(),
        elapsed_ms: 0,
        removed: false,
    };
    card.field_on();
    if ANTICOLLISION_MS > dwell_ms {
        run.removed = true;
    } else {
        run.elapsed_ms = ANTICOLLISION_MS;
    }
    for op in ops {
        if run.removed {
            run.ops
                .push(serde_json::json!({ "op": op.name(), "status": "not_run" }));
            continue;
        }
        let frames = match op {
            TapOp::ReadNdef => vec![vec![cmd::FAST_READ, USER_FIRST, USER_LAST]],
            TapOp::WriteNdef(records) => match ndef_write_frames(&card, records, "writeNdef") {
                Ok(frames) => frames,
                Err(e) => {
                    run.ops.push(serde_json::json!({ "op": op.name(), "status": "failed", "error": e.message }));
                    continue;
                }
            },
            TapOp::Raw(frames) => frames.clone(),
        };
        let mut responses = Vec::new();
        let mut rendered = Vec::new();
        let mut status = "ok";
        for frame in &frames {
            if run.elapsed_ms + FRAME_MS > dwell_ms {
                status = "removed";
                run.removed = true;
                break;
            }
            run.elapsed_ms += FRAME_MS;
            // A READ does not change the card, so the mask before it covers the bytes it answers
            // with.
            let card_mask = secret_mask(&card, values);
            let response = card.command(frame);
            run.frames.push(frame.clone());
            let nak = matches!(response, CardResponse::Nak(_));
            rendered.push(response_hex(frame, &response, &card_mask, values));
            responses.push(response);
            // A phone's NDEF read or write stops at the first NAK; a raw console sends every frame.
            if nak && !matches!(op, TapOp::Raw(_)) {
                status = "failed";
                break;
            }
        }
        let mut report = serde_json::json!({ "op": op.name(), "status": status });
        match op {
            TapOp::ReadNdef => match responses.first() {
                Some(CardResponse::Data(area)) if status == "ok" => {
                    report["records"] = match ndef::find_ndef(area).and_then(|tlv| {
                        let bytes = area
                            .get(tlv.value..tlv.value + tlv.len)
                            .ok_or(pemu_board::ntag213::CardError::MalformedTlv)?;
                        ndef::decode_message(bytes)
                    }) {
                        Ok(records) => {
                            serde_json::Value::Array(records.iter().map(record_json).collect())
                        }
                        Err(e) => {
                            report["status"] = "failed".into();
                            report["error"] = format!("{e:?}").into();
                            serde_json::Value::Array(Vec::new())
                        }
                    };
                }
                Some(_) => report["responses"] = rendered.clone().into(),
                None => {}
            },
            TapOp::WriteNdef(_) => {
                report["pages_written"] = responses
                    .iter()
                    .filter(|r| **r == CardResponse::Ack)
                    .count()
                    .into();
                if let Some(nak) = responses.iter().find(|r| matches!(r, CardResponse::Nak(_))) {
                    report["responses"] = vec![hex(&nak.to_bytes())].into();
                }
            }
            TapOp::Raw(_) => {
                report["responses"] = rendered.into();
            }
        }
        run.ops.push(report);
    }
    card.field_off();
    run.card = card;
    run
}

pub fn nfc_tap_on(session: &mut Session, args: &NfcTapArgs) -> Result<Output, ApiError> {
    let before = read_card(session)?;
    // The ops' secret inputs mask this output; the store takes them from the journaled tap at its
    // next extension. A frame the dwell cuts off still counts, which errs on the side of masking.
    let mut user = UserSecrets::default();
    for op in &args.ops {
        match op {
            TapOp::WriteNdef(records) => user.add_records(records),
            TapOp::Raw(frames) => user.add_frames(frames),
            TapOp::ReadNdef => {}
        }
    }
    let mut run = run_tap(
        before.clone(),
        &args.ops,
        args.dwell_ms,
        &secret_values(&card_secrets(session, &before, &user)),
    );
    // A Wi-Fi record written by raw frames is only a key once it is on the card; the tap is pure,
    // so it runs again with the key in the set.
    let mut written = UserSecrets::default();
    written.add_card(&run.card);
    if user.lacks_any_of(&written) {
        user.merge(&written);
        run = run_tap(
            before.clone(),
            &args.ops,
            args.dwell_ms,
            &secret_values(&card_secrets(session, &before, &user)),
        );
    }
    journal_tap(session, &run.frames, &run.card)?;
    let receipt = session.receipt();
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "uid": SECRET,
        "counter": run.card.counter(),
        "dwell_ms": args.dwell_ms,
        "elapsed_ms": run.elapsed_ms,
        "removed": run.removed,
        "frames": run.frames.len(),
        "ops": run.ops,
        "notes": [
            "firmware-visible: none (the card talks to the reader, not to the MCU)",
            "the frame timing is class C: 10 ms anticollision and 5 ms a frame (UNVERIFIED)",
        ],
    });
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} nfc_tap {} op(s), {} frame(s) in {} ms of {} ms{} counter={}",
        json["instance"].as_str().unwrap_or("?"),
        args.ops.len(),
        run.frames.len(),
        run.elapsed_ms,
        args.dwell_ms,
        if run.removed { ", removed" } else { "" },
        run.card.counter(),
    );
    for op in &run.ops {
        let detail = if op.get("records").is_some() {
            records_text(&op["records"])
        } else if let Some(responses) = op["responses"].as_array() {
            responses
                .iter()
                .filter_map(|r| r.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        } else if let Some(n) = op["pages_written"].as_u64() {
            format!("{n} page(s) written")
        } else {
            String::new()
        };
        let _ = writeln!(
            text,
            "  {} {} {}",
            op["op"].as_str().unwrap_or("?"),
            op["status"].as_str().unwrap_or("?"),
            detail
        );
    }
    let output =
        Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT);
    Ok(redacted(session, &run.card, &user, output))
}

/// Web `NdefRecord`.
pub(crate) fn record_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["type"],
        "properties": {
            "type": { "type": "string", "enum": ["uri", "text", "wifi"] },
            "uri": { "type": "string" },
            "text": { "type": "string" },
            "lang": { "type": "string" },
            "ssid": { "type": "string" },
            "auth": { "type": "string", "enum": ["open", "wpa-personal", "wpa", "wpa2-personal", "wpa2"] },
            "encr": { "type": "string", "enum": ["none", "wep", "tkip", "aes"] },
            "key": { "type": "string" }
        }
    })
}

pub fn input_schema() -> Schema {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["ops"],
        "description": "`nfc_tap` arguments.",
        "properties": {
            "instance": instance_schema(),
            "ops": {
                "type": "array",
                "minItems": 1,
                "maxItems": OPS_MAX,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["op"],
                    "properties": {
                        "op": { "type": "string", "enum": ["readNdef", "writeNdef", "raw"] },
                        "ndef": { "type": "array", "items": record_schema() },
                        "frames": { "type": "array", "items": { "type": "string" }, "description": "Hex frames, no CRC." }
                    }
                }
            },
            "dwell_ms": { "type": "integer", "minimum": 0, "maximum": DWELL_MS_MAX, "description": "Phone in field (300)." }
        }
    });
    Schema::try_from(schema).unwrap_or_default()
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "uid": { "type": "string" },
            "counter": { "type": "integer" },
            "dwell_ms": { "type": "integer" },
            "elapsed_ms": { "type": "integer" },
            "removed": { "type": "boolean" },
            "frames": { "type": "integer" },
            "ops": { "type": "array", "items": { "type": "object" } },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Tap the NFC card with a virtual phone: read or write NDEF, or send raw frames.
#[command(
    api_crate = crate,
    name = "nfc_tap",
    group = nfc,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "nfc.tap",
    errors(E_USAGE, E_STATE, E_LEASE, E_SNAPSHOT, E_INTERNAL),
    example(
        title = "Read the NDEF records",
        args = r#"{"ops":[{"op":"readNdef"}]}"#,
    ),
    example(
        title = "Write a URI record",
        args = r#"{"ops":[{"op":"writeNdef","ndef":[{"type":"uri","uri":"https://example.com/p"}]}]}"#,
    ),
    example(
        title = "Send GET_VERSION and READ 4 as raw frames",
        args = r#"{"ops":[{"op":"raw","frames":["60","3004"]}]}"#,
    ),
)]
pub fn nfc_tap(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = NfcTapArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_NFC_TAP.annotations, args.instance.as_deref()),
        |session| nfc_tap_on(session, &args),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use pemu_core::time::VTime;

    use crate::commands::start::{Boot, StartArgs};

    /// The card is only reachable through a snapshot of a real `Machine`, so a double cannot stand
    /// in.
    pub(crate) fn real_pool() -> (Pool, InstanceId) {
        use pemu_loader::bundle::FlashImage;
        use pemu_loader::efuse_image::EfuseImage;
        use pemu_machine::config::{Assets, MachineConfig};

        let assets =
            Assets::with_bundled_rom(FlashImage::erased(), None, None, EfuseImage::synth(0))
                .expect("the bundled ROM is pinned");
        let machine = pemu_machine::Machine::new(MachineConfig::default(), assets)
            .expect("the ROM fits the ROM window");
        let mut pool = Pool::new();
        let start = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&start, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("the instance was just created")
            .transition(crate::instance::Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        (pool, id)
    }

    pub(crate) fn tap(
        pool: &mut Pool,
        id: InstanceId,
        json: serde_json::Value,
    ) -> Result<Output, ApiError> {
        let args = NfcTapArgs::from_json(&json)?;
        let session = pool.session_mut(id).expect("a session");
        nfc_tap_on(session, &args)
    }

    const URI: &str = "https://example.com/p";

    #[test]
    fn the_wifi_payload_is_the_example_wsc_payload_byte_for_byte() {
        let payload = wifi_payload("TestAP", 0x0020, 0x0008, "password1");
        assert_eq!(payload.len(), 69, "the example WSC payload is 69 bytes");
        let message = ndef::encode_message(&[NdefRecord::Mime {
            mime_type: WSC_MIME.to_owned(),
            payload: payload.clone(),
        }]);
        assert_eq!(&message[0..3], &[0xD2, 0x17, 0x45]);
        assert_eq!(message.len(), 95);
        assert_eq!(
            &payload[0..9],
            &[0x10, 0x4A, 0x00, 0x01, 0x10, 0x10, 0x0E, 0x00, 0x32]
        );
        let wifi = wifi_of(&payload).expect("it parses back");
        assert_eq!(
            wifi,
            (
                "TestAP".to_owned(),
                "wpa2-personal".to_owned(),
                "aes".to_owned()
            )
        );
    }

    #[test]
    fn a_write_ndef_op_is_write_frames_and_a_read_ndef_op_reads_it_back() {
        let (mut pool, id) = real_pool();
        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"writeNdef","ndef":[{"type":"uri","uri":URI}]},{"op":"readNdef"}]}),
        )
        .expect("the tap runs");
        assert_eq!(out.json["removed"], false);
        assert_eq!(out.json["ops"][0]["status"], "ok", "{}", out.json);
        assert!(out.json["ops"][0]["pages_written"].as_u64().unwrap_or(0) >= 5);
        assert_eq!(out.json["ops"][1]["records"][0]["uri"], URI);
        let again = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"}]}),
        )
        .expect("tap");
        assert_eq!(again.json["ops"][0]["records"][0]["uri"], URI);
        let session = pool.session_mut(id).expect("a session");
        assert_eq!(
            session.now(),
            VTime(0),
            "a tap runs no instruction and takes no virtual time"
        );
    }

    #[test]
    fn a_raw_op_reports_every_response_and_a_nak_does_not_stop_it() {
        let (mut pool, id) = real_pool();
        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"raw","frames":["60","A2 00 00000000","3A0404"]}]}),
        )
        .expect("the tap runs");
        let responses = &out.json["ops"][0]["responses"];
        assert_eq!(responses[0], "0004040201000F03", "GET_VERSION");
        assert_eq!(responses[1], "00", "a WRITE to page 0 is a NAK 0h");
        assert_eq!(
            responses[2], "0103A00C",
            "FAST_READ 04 04: the delivery Lock Control TLV"
        );
    }

    #[test]
    fn a_dwell_too_short_fails_like_a_removal_and_journals_only_what_ran() {
        let (mut pool, id) = real_pool();
        let ops = serde_json::json!([{"op":"writeNdef","ndef":[{"type":"uri","uri":URI}]},{"op":"readNdef"}]);
        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops": ops, "dwell_ms": ANTICOLLISION_MS + 2 * FRAME_MS}),
        )
        .expect("the tap runs");
        assert_eq!(out.json["removed"], true);
        assert_eq!(out.json["ops"][0]["status"], "removed");
        assert_eq!(out.json["ops"][0]["pages_written"], 2);
        assert_eq!(out.json["ops"][1]["status"], "not_run");
        // The message is torn, not absent.
        let read = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"}]}),
        )
        .expect("tap");
        assert_ne!(read.json["ops"][0]["records"][0]["uri"], URI);
        let none = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"}],"dwell_ms":0}),
        )
        .expect("tap");
        assert_eq!(none.json["ops"][0]["status"], "not_run");
        assert_eq!(none.json["frames"], 0);
    }

    #[test]
    fn the_uid_never_reaches_the_output() {
        let (mut pool, id) = real_pool();
        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"raw","frames":["3000","3A0003"]}]}),
        )
        .expect("tap");
        let session = pool.session_mut(id).expect("a session");
        let card = read_card(session).expect("the card");
        let uid = card.uid();
        let rendered = format!("{} {}", out.json, out.text).to_uppercase();
        assert_eq!(out.json["uid"], SECRET);
        // Nor either half of it around BCC0.
        for part in [&uid[..], &uid[0..3], &uid[3..7]] {
            assert!(!rendered.contains(&hex(part)), "{rendered}");
        }
        // READ 00: pages 0 and 1 and BCC1 masked, the rest of page 2 and the CC page kept.
        assert_eq!(
            out.json["ops"][0]["responses"][0],
            format!("{SECRET}000000E1101200")
        );
        assert_eq!(
            out.json["ops"][0]["responses"][1],
            format!("{SECRET}000000E1101200")
        );
        let set = card_secrets(session, &card, &UserSecrets::default());
        assert!(
            set.members()
                .iter()
                .any(|m| m.kind == crate::secret_set::MemberKind::NfcUid)
        );
        // So a synthetic card does not make the instance unexportable.
        let stored = session.with_store(|store| store.secret_set(id).clone());
        assert!(
            !stored
                .members()
                .iter()
                .any(|m| m.kind == crate::secret_set::MemberKind::NfcUid)
        );
    }

    const KEY: &str = "correct-horse-battery";

    /// So a key split across pages or answers is caught too.
    fn key_windows() -> Vec<String> {
        KEY.as_bytes().windows(4).map(hex).collect()
    }

    fn wifi(key: &str) -> serde_json::Value {
        serde_json::json!([{"type":"wifi","ssid":"lab","auth":"wpa2","encr":"aes","key":key}])
    }

    #[test]
    fn a_raw_fast_read_after_a_wifi_write_shows_no_key_hex() {
        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = crate::commands::snapshot::tests::real_instance();
        let same = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[
                {"op":"writeNdef","ndef":wifi(KEY)},
                {"op":"raw","frames":["3A0427"]}
            ]}),
        )
        .expect("the tap runs");
        let later = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"raw","frames":["3A0427","3008","300A"]}]}),
        )
        .expect("the tap runs");
        for out in [&same, &later] {
            let rendered = format!("{} {}", out.json, out.text).to_uppercase();
            assert!(rendered.contains(SECRET), "{rendered}");
            for window in key_windows() {
                assert!(!rendered.contains(&window), "{window} in {rendered}");
            }
        }
        // The delivery Lock Control TLV opens page 4.
        assert!(
            later.json["ops"][0]["responses"][0]
                .as_str()
                .is_some_and(|r| r.starts_with("0103A00C")),
            "{}",
            later.json
        );
        let stored =
            crate::commands::snapshot::secrets_of(pool.session_mut(id).expect("a session"));
        assert!(
            stored.members().iter().any(|m| m.bytes == KEY.as_bytes()),
            "the key joined the store's set"
        );
        assert!(pool.with_store(|store| store.is_tainted(id)));
    }

    #[test]
    fn a_wifi_record_written_by_raw_frames_is_a_secret_input_too() {
        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = crate::commands::snapshot::tests::real_instance();
        let card = read_card(pool.session_mut(id).expect("a session")).expect("the card");
        let records = records_from_json(&wifi(KEY), "ndef").expect("a record");
        let mut frames: Vec<String> = ndef_write_frames(&card, &records, "ndef")
            .expect("it fits")
            .iter()
            .map(|f| hex(f))
            .collect();
        frames.push("3A0427".to_owned());
        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"raw","frames":frames}]}),
        )
        .expect("the tap runs");
        let rendered = format!("{} {}", out.json, out.text).to_uppercase();
        for window in key_windows() {
            assert!(!rendered.contains(&window), "{window} in {rendered}");
        }
        let stored =
            crate::commands::snapshot::secrets_of(pool.session_mut(id).expect("a session"));
        assert!(stored.members().iter().any(|m| m.bytes == KEY.as_bytes()));
    }

    #[test]
    fn export_refuses_after_a_user_pwd_write_and_allows_a_pure_seed_card() {
        use crate::commands::snapshot::{snapshot_on_pool, tests::args};
        use crate::error::E_SECRET_REFUSED;

        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = crate::commands::snapshot::tests::real_instance();
        tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"},{"op":"raw","frames":["3000","3A0003"]}]}),
        )
        .expect("the tap runs");
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"seed"})),
        )
        .expect("a seed card is not a secret input");
        assert_eq!(out.json["redacted"], true);

        let out = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"raw","frames":["A22B5A17C3E9","1B5A17C3E9","302B"]}]}),
        )
        .expect("the tap runs");
        let rendered = format!("{} {}", out.json, out.text).to_uppercase();
        assert!(!rendered.contains("5A17C3E9"), "{rendered}");
        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"user"})),
        )
        .expect_err("a user PWD taints the instance");
        assert_eq!(error.code, E_SECRET_REFUSED);
        pool.with_store(|store| store.forget(id));

        // A fork carries the card, so it is secret-bearing too.
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"f","count":1})),
        )
        .expect("fork");
        let fork = InstanceId::parse(out.json["instances"][0].as_str().expect("an id"))
            .expect("a minted id");
        crate::commands::snapshot::secrets_of(pool.session_mut(fork).expect("the fork"));
        assert!(pool.with_store(|store| store.is_tainted(fork)));
    }

    #[test]
    fn a_key_the_set_cannot_hold_is_refused() {
        for key in ["abc", "aaaaaaaa"] {
            let error = records_from_json(&wifi(key), "ndef").expect_err("refused");
            assert_eq!(error.code, E_USAGE, "{key}");
        }
        records_from_json(&wifi(""), "ndef").expect("an open network has no key");
    }

    #[test]
    fn arguments_outside_the_schema_are_usage_errors() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"ops":[]}),
            serde_json::json!({"ops":[{"op":"fly"}]}),
            serde_json::json!({"ops":[{"op":"raw","frames":["3"]}]}),
            serde_json::json!({"ops":[{"op":"writeNdef","ndef":[{"type":"wifi","ssid":"x","auth":"wep2"}]}]}),
            serde_json::json!({"ops":[{"op":"readNdef"}],"dwell_ms":DWELL_MS_MAX + 1}),
        ] {
            let error = NfcTapArgs::from_json(&bad).expect_err("refused");
            assert_eq!(error.code, E_USAGE, "{bad}");
        }
    }
}
