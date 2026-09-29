//! `passportsim nfc_tag`: set up the NTAG213 on the board and dump it.
//!
//! The card's only input is `InputEvent::NfcTap`, so a load is one tap whose frames are WRITE
//! commands, checked against a copy of the card first ([`super::nfc_tap`]). A frame the copy
//! refuses (a locked page, a page above AUTH0) refuses the whole call with `E_STATE`. In order:
//!
//! 1. `ndef`: the user pages the new message changes (keeping the delivery Lock Control TLV);
//! 2. `counter: true`: NFC_CNT_EN in ACCESS (page 0x2A bit 4), without which the NFC counter never
//!    moves;
//! 3. `lock: true`: the CC write-access byte to 0Fh, the dynamic lock bytes of page 0x28, then the
//!    static lock bytes of page 2, all OTP; only the `card` domain reset undoes it. The CC write
//!    goes first because L3 locks page 3.
//!
//! `uid` is refused: pages 0 and 1 are read-only and the UID comes from the machine seed. The
//! summary never carries the UID, PWD or PACK; `dump: true` adds the 45 pages as hex with those
//! bytes and every `SecretSet` member as `<SECRET>`. A Wi-Fi key in `ndef` is a secret input and
//! taints the instance.

use std::fmt::Write as _;

use pemu_board::ntag213::{CardResponse, Ntag213, PAGE_BYTES, PAGE_DYN_LOCK, PAGE_MAX};

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_SNAPSHOT, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::nfc_tap::{
    SECRET, UserSecrets, bind_checked, card_secrets, counter_enable_frame, hex, journal_tap,
    masked_hex, ndef_write_frames, read_card, record_schema, records_from_json, records_text,
    redacted, secret_mask, secret_values, tag_json, write_frame,
};
use crate::args::{instance_schema, object, only, opt_bool, opt_str, usage};
use crate::session::Session;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NfcTagArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub ndef: Option<Vec<pemu_board::ntag213::ndef::NdefRecord>>,
    pub counter: bool,
    /// Irreversible.
    pub lock: bool,
    pub dump: bool,
}

impl NfcTagArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<NfcTagArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &["instance", "ndef", "uid", "lock", "counter", "dump"],
        )?;
        if args.get("uid").is_some_and(|v| !v.is_null()) {
            return Err(usage(
                "uid",
                "the UID is drawn from the machine seed and pages 0 and 1 are read-only",
            )
            .with_hint("start the instance with another `seed` for another synthetic UID"));
        }
        let ndef = match args.get("ndef") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(records_from_json(value, "ndef")?),
        };
        Ok(NfcTagArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            ndef,
            counter: opt_bool(args, "counter")?.unwrap_or(false),
            lock: opt_bool(args, "lock")?.unwrap_or(false),
            dump: opt_bool(args, "dump")?.unwrap_or(false),
        })
    }
}

/// In the module docs' order.
fn load_frames(card: &Ntag213, args: &NfcTagArgs) -> Result<Vec<Vec<u8>>, ApiError> {
    let mut frames = Vec::new();
    if let Some(records) = &args.ndef {
        frames.extend(ndef_write_frames(card, records, "ndef")?);
    }
    if args.counter && !card.counter_enabled() {
        frames.push(counter_enable_frame(card));
    }
    if args.lock {
        // Page 3 is OTP, so the write ORs 0Fh in.
        frames.push(write_frame(3, [0, 0, 0, 0x0F]));
        frames.push(write_frame(PAGE_DYN_LOCK, [0xFF, 0xFF, 0xFF, 0x00]));
        frames.push(write_frame(2, [0, 0, 0xFF, 0xFF]));
    }
    Ok(frames)
}

/// The UID, BCC1, PWD and PACK positions and every occurrence of `values` are `<SECRET>`.
fn pages_json(card: &Ntag213, values: &[Vec<u8>]) -> Vec<String> {
    let mask = secret_mask(card, values);
    (0..=PAGE_MAX)
        .map(|page| {
            let bytes = card.page(page).unwrap_or([0; PAGE_BYTES]);
            let at = page as usize * PAGE_BYTES;
            masked_hex(&bytes, &mask[at..at + PAGE_BYTES])
        })
        .collect()
}

pub fn nfc_tag_on(session: &mut Session, args: &NfcTagArgs) -> Result<Output, ApiError> {
    let card = read_card(session)?;
    let frames = load_frames(&card, args)?;
    // The key masks this output; the store takes it from the journaled card at its next extension.
    let mut user = UserSecrets::default();
    if let Some(records) = &args.ndef {
        user.add_records(records);
    }
    let mut after = card.clone();
    if !frames.is_empty() {
        after.field_on();
        for frame in &frames {
            let response = after.command(frame);
            if response != CardResponse::Ack {
                return Err(ApiError::new(
                    E_STATE,
                    format!(
                        "the card refused the WRITE of page 0x{:02X} with {}; nothing was written",
                        frame[1],
                        hex(&response.to_bytes()),
                    ),
                )
                .with_hint(
                    "`nfc_tag` with no arguments shows `locked_pages` and the counter state",
                ));
            }
        }
        after.field_off();
        journal_tap(session, &frames, &after)?;
    }
    let receipt = session.receipt();
    let tag = tag_json(&after);
    let mut json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "applied": {
            "ndef": args.ndef.as_ref().map_or(0, Vec::len),
            "counter": args.counter,
            "lock": args.lock,
        },
        "frames": frames.len(),
        "tag": tag,
        "notes": ["firmware-visible: none (the card talks to a reader, not to the MCU)"],
    });
    if args.dump {
        json["pages"] = pages_json(
            &after,
            &secret_values(&card_secrets(session, &after, &user)),
        )
        .into();
    }
    let mut text = String::new();
    let _ = writeln!(
        text,
        "{} nfc_tag {} frame(s): NTAG213 uid={SECRET} counter={}{} {} of {} user bytes; {}",
        json["instance"].as_str().unwrap_or("?"),
        frames.len(),
        after.counter(),
        if after.counter_enabled() {
            " (enabled)"
        } else {
            ""
        },
        json["tag"]["ndef_bytes_used"],
        json["tag"]["user_bytes"],
        records_text(&json["tag"]["records"]),
    );
    if let Some(pages) = json["pages"].as_array() {
        for (i, page) in pages.iter().enumerate() {
            let _ = writeln!(text, "  {i:02X}: {}", page.as_str().unwrap_or(""));
        }
    }
    let output =
        Output::new(json, text.trim_end().to_owned(), receipt).shaped(&ShapeLimits::DEFAULT);
    Ok(redacted(session, &after, &user, output))
}

/// Web contract `NfcTagArgs`.
pub fn input_schema() -> Schema {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "`nfc_tag` arguments; none dumps the card.",
        "properties": {
            "instance": instance_schema(),
            "ndef": { "type": "array", "minItems": 1, "items": record_schema() },
            "uid": { "type": "string", "description": "Refused: seed-derived." },
            "lock": { "type": "boolean", "description": "Irreversible read-only lock." },
            "counter": { "type": "boolean", "description": "Enable the NFC counter." },
            "dump": { "type": "boolean", "description": "Add the 45 pages." }
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
            "applied": { "type": "object" },
            "frames": { "type": "integer" },
            "tag": { "type": "object" },
            "pages": { "type": "array", "items": { "type": "string" } },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Load NDEF records onto the NFC card, enable its counter or lock it, and dump it.
#[command(
    api_crate = crate,
    name = "nfc_tag",
    group = nfc,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "nfc.tag",
    errors(E_USAGE, E_STATE, E_LEASE, E_SNAPSHOT, E_INTERNAL),
    example(
        title = "Load a URI record and enable the NFC counter",
        args = r#"{"ndef":[{"type":"uri","uri":"https://example.com/p"}],"counter":true}"#,
    ),
    example(
        title = "Dump the card with the UID redacted",
        args = r#"{"dump":true}"#,
    ),
)]
pub fn nfc_tag(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = NfcTagArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_NFC_TAG.annotations, args.instance.as_deref()),
        |session| nfc_tag_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_board::ntag213::PAGE_PWD;

    use crate::commands::nfc_tap::tests::{real_pool, tap};
    use crate::instance::InstanceId;
    use crate::pool::Pool;

    const URI: &str = "https://example.com/p";

    fn tag(pool: &mut Pool, id: InstanceId, json: serde_json::Value) -> Result<Output, ApiError> {
        let args = NfcTagArgs::from_json(&json)?;
        nfc_tag_on(pool.session_mut(id).expect("a session"), &args)
    }

    /// A URI loaded then tapped reads back, the NFC counter increments, and the dump redacts the
    /// UID.
    #[test]
    fn a_uri_loaded_then_tapped_reads_back_counts_and_dumps_redacted() {
        let (mut pool, id) = real_pool();
        let loaded = tag(
            &mut pool,
            id,
            serde_json::json!({"ndef":[{"type":"uri","uri":URI}],"counter":true}),
        )
        .expect("the load runs");
        assert_eq!(
            loaded.json["tag"]["records"][0]["uri"], URI,
            "{}",
            loaded.json
        );
        assert_eq!(loaded.json["tag"]["counter_enabled"], true);
        assert_eq!(loaded.json["tag"]["counter"], 0, "WRITEs do not count");

        let first = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"}]}),
        )
        .expect("tap");
        assert_eq!(first.json["ops"][0]["records"][0]["uri"], URI);
        assert_eq!(first.json["counter"], 1);
        let second = tap(
            &mut pool,
            id,
            serde_json::json!({"ops":[{"op":"readNdef"},{"op":"readNdef"}]}),
        )
        .expect("tap");
        assert_eq!(
            second.json["counter"], 2,
            "the counter moves once per field pass"
        );

        let dump = tag(&mut pool, id, serde_json::json!({"dump":true})).expect("the dump runs");
        assert_eq!(dump.json["frames"], 0, "a dump journals nothing");
        let card = read_card(pool.session_mut(id).expect("a session")).expect("the card");
        let uid = card.uid();
        let rendered = format!("{} {}", dump.json, dump.text).to_uppercase();
        for part in [&uid[..], &uid[0..3], &uid[3..7]] {
            assert!(!rendered.contains(&hex(part)), "{rendered}");
        }
        assert_eq!(dump.json["tag"]["uid"], SECRET);
        assert_eq!(dump.json["pages"][0], SECRET);
        assert_eq!(dump.json["pages"][1], SECRET);
        assert_eq!(dump.json["pages"][PAGE_PWD as usize], SECRET);
        assert_eq!(dump.json["pages"][3], "E1101200");
    }

    #[test]
    fn a_wifi_and_a_text_record_round_trip_with_the_key_redacted() {
        // The key taints the instance, which the snapshot tests' shared store must not see.
        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = real_pool();
        let out = tag(
            &mut pool,
            id,
            serde_json::json!({"ndef":[
                {"type":"wifi","ssid":"TestAP","auth":"wpa2","encr":"aes","key":"password1"},
                {"type":"text","text":"hi"}
            ]}),
        )
        .expect("the load runs");
        let records = &out.json["tag"]["records"];
        assert_eq!(records[0]["type"], "wifi", "{}", out.json);
        assert_eq!(records[0]["ssid"], "TestAP");
        assert_eq!(records[0]["auth"], "wpa2-personal");
        assert_eq!(records[0]["key"], SECRET);
        assert_eq!(
            records[1],
            serde_json::json!({"type":"text","text":"hi","lang":"en"})
        );
        assert!(!format!("{} {}", out.json, out.text).contains("password1"));
    }

    /// Masked so the page strings put back together carry no part of it.
    #[test]
    fn a_dump_reassembled_from_its_pages_carries_no_wifi_key() {
        const KEY: &str = "correct-horse-battery";
        let _world = crate::commands::snapshot::tests::world();
        let (mut pool, id) = crate::commands::snapshot::tests::real_instance();
        let out = tag(
            &mut pool,
            id,
            serde_json::json!({"ndef":[{"type":"wifi","ssid":"lab","auth":"wpa2","encr":"aes","key":KEY}],"dump":true}),
        )
        .expect("the load runs");
        let pages: Vec<&str> = out.json["pages"]
            .as_array()
            .expect("45 pages")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect();
        assert_eq!(pages.len(), 45);
        let joined = pages.concat().replace(SECRET, "");
        let masked = pages.iter().filter(|p| p.contains(SECRET)).count();
        assert!(masked >= 2 + 6, "UID pages and the key's pages: {pages:?}");
        for window in KEY.as_bytes().windows(4) {
            assert!(!joined.contains(&hex(window)), "{joined}");
            assert!(
                !out.text.to_uppercase().contains(&hex(window)),
                "{}",
                out.text
            );
        }
    }

    #[test]
    fn a_lock_is_journaled_and_a_later_load_is_refused_whole() {
        let (mut pool, id) = real_pool();
        let locked = tag(&mut pool, id, serde_json::json!({"lock":true})).expect("the lock runs");
        assert_eq!(locked.json["tag"]["cc_hex"], "E110120F");
        let pages = locked.json["tag"]["locked_pages"]
            .as_array()
            .expect("a list")
            .len();
        assert_eq!(pages, 37, "pages 3 to 39: {}", locked.json);
        let error = tag(
            &mut pool,
            id,
            serde_json::json!({"ndef":[{"type":"uri","uri":URI}]}),
        )
        .expect_err("a locked tag refuses");
        assert_eq!(error.code, E_STATE);
        let dump = tag(&mut pool, id, serde_json::json!({})).expect("a dump");
        assert_eq!(
            dump.json["tag"]["records"],
            serde_json::json!([]),
            "nothing was written"
        );
    }

    #[test]
    fn a_uid_is_refused() {
        let error = NfcTagArgs::from_json(&serde_json::json!({"uid":"04A1B2C3D4E5F6"}))
            .expect_err("refused");
        assert_eq!(error.code, E_USAGE);
    }
}
