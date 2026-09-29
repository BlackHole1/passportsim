//! `passportsim env`: the world around the device: battery, USB cable and client, NFC card,
//! microphone source, Wi-Fi air and the external HCI bridge. BLE peers and LAN services have their
//! own commands.
//!
//! Every change is a journaled input handed to [`pemu_machine::MachineApi::input`], so a replay
//! replays the environment. `env` does not advance virtual time: a command that ran the guest would
//! make a scenario's time budget depend on how many `env` steps it had.
//!
//! `effects` says when the firmware cannot observe a change, so an agent does not assert on what
//! the device cannot see. USB is the case that matters: `charger` and `host` are the same U-state
//! to the firmware ([`UsbWorld::firmware_visible`]), while `unplugged` is U0, where SOF stops and
//! `FRAME_NUM` freezes.

use std::fmt::Write as _;

use pemu_core::input::{
    BatterySet, EnvChange, InputEvent, MicSource, NfcOp, WIFI_MAX_AUTH, WIFI_MAX_CHANNEL,
    WIFI_MAX_PSK, WIFI_MAX_SSID, WIFI_MIN_PSK, WifiAp, WifiApError,
};
use pemu_machine::machine::At;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STATE, E_USAGE};
use crate::instance::Lifecycle;
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use crate::args::{JsonMap, instance_schema, object, only, opt_bool, opt_str, req_str, usage};
use crate::pool::{Pool, with_pool};
use crate::session::Session;

/// In millivolts.
pub const BATTERY_MV_MIN: u64 = 2500;
pub const BATTERY_MV_MAX: u64 = 4400;
/// In whole degrees Celsius.
pub const BATTERY_TEMP_C_MIN: i64 = -20;
pub const BATTERY_TEMP_C_MAX: i64 = 80;
/// Above the 8 kHz Nyquist limit of the 16 kHz capture path a tone aliases, so a higher number is a
/// mistake, not a test.
pub const MIC_TONE_HZ_MAX: u64 = 8000;

/// Two journaled facts: cable in, and a host client holding the port. The names are the host's
/// view; two of them collapse into one U-state once the MCU rail is on
/// ([`UsbWorld::firmware_visible`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum UsbWorld {
    /// U0 DETACHED: no SOF, the IN FIFO never drains.
    Unplugged,
    /// VBUS present, nothing enumerates.
    Charger,
    /// Enumerated, no port open: U2 ATTACHED_IDLE.
    Host,
    /// U3 ATTACHED_OPEN.
    Open,
}

impl UsbWorld {
    pub fn parse(text: &str) -> Option<UsbWorld> {
        match text {
            "unplugged" => Some(UsbWorld::Unplugged),
            "charger" => Some(UsbWorld::Charger),
            "host" => Some(UsbWorld::Host),
            "open" => Some(UsbWorld::Open),
            _ => None,
        }
    }

    /// As written in an argument and reported in `applied`.
    pub fn as_str(self) -> &'static str {
        match self {
            UsbWorld::Unplugged => "unplugged",
            UsbWorld::Charger => "charger",
            UsbWorld::Host => "host",
            UsbWorld::Open => "open",
        }
    }

    /// `(cable plugged, client open)`. `charger` and `host` differ only in enumeration, which is
    /// the model's own state and not a journaled input, so both journal a plugged cable with no
    /// client. They stay apart in the vocabulary because the host behaves differently.
    pub fn events(self) -> [InputEvent; 2] {
        let (cable, client) = match self {
            UsbWorld::Unplugged => (false, false),
            UsbWorld::Charger | UsbWorld::Host => (true, false),
            UsbWorld::Open => (true, true),
        };
        [
            InputEvent::UsbCable { plugged: cable },
            InputEvent::UsbClient { open: client },
        ]
    }

    /// The firmware has no VBUS or charge-state signal.
    pub fn firmware_visible(self, rail_on: bool) -> &'static str {
        match (self, rail_on) {
            (UsbWorld::Unplugged, _) => "usb: U0 DETACHED, the IN FIFO stops draining",
            (UsbWorld::Charger | UsbWorld::Host, false) => {
                "usb: U1 CHARGE_ONLY, the MCU rail is off"
            }
            (UsbWorld::Charger | UsbWorld::Host, true) => {
                "usb: U2 ATTACHED_IDLE; firmware cannot tell a charger from an idle host"
            }
            (UsbWorld::Open, _) => "usb: U3 ATTACHED_OPEN, the IN FIFO drains",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MicArg {
    pub source: MicSource,
}

impl MicArg {
    /// The fields that kind needs.
    fn from_json(value: &serde_json::Value) -> Result<MicArg, ApiError> {
        let args = value
            .as_object()
            .ok_or_else(|| usage("mic", "expected an object with a `kind`"))?;
        only(args, &["kind", "hz", "amplitude", "name"])?;
        let source = match req_str(args, "kind")? {
            "silence" => MicSource::Silence,
            "tone" => {
                let hz = u32::try_from(int_in(args, "hz", 1, MIC_TONE_HZ_MAX)?)
                    .map_err(|_| usage("mic.hz", "does not fit"))?;
                let amplitude =
                    i16::try_from(signed_in(args, "amplitude", 0, i64::from(i16::MAX))?)
                        .map_err(|_| usage("mic.amplitude", "does not fit"))?;
                MicSource::Tone { hz, amplitude }
            }
            "file" => MicSource::File {
                name: req_str(args, "name")?.to_owned(),
            },
            "live" => MicSource::Live,
            other => {
                return Err(usage(
                    "mic.kind",
                    &format!("`{other}` is not one of silence, tone, file, live"),
                ));
            }
        };
        Ok(MicArg { source })
    }

    fn name(&self) -> &'static str {
        match self.source {
            MicSource::Silence => "silence",
            MicSource::Tone { .. } => "tone",
            MicSource::File { .. } => "file",
            MicSource::Live => "live",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    /// Every field is optional on its own.
    pub battery: Option<BatterySet>,
    pub usb: Option<UsbWorld>,
    /// Whether a card sits in the reader field.
    pub nfc_card: Option<bool>,
    /// From now on.
    pub mic: Option<MicArg>,
    /// Replacing the ones set before. A key inside one is a secret input and never reaches an
    /// output.
    pub wifi_aps: Option<Vec<WifiAp>>,
    /// Attach or detach the external HCI bridge. The machinery lives in `commands::ble_scan`.
    pub ble_bridge: Option<bool>,
}

impl EnvArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<EnvArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "battery",
                "usb",
                "nfc",
                "mic",
                "wifi",
                "ble_bridge",
            ],
        )?;
        let battery = match args.get("battery") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(battery_from_json(value)?),
        };
        let usb = match opt_str(args, "usb")? {
            None => None,
            Some(text) => Some(UsbWorld::parse(text).ok_or_else(|| {
                usage(
                    "usb",
                    &format!("`{text}` is not one of unplugged, charger, host, open"),
                )
            })?),
        };
        let nfc = match args.get("nfc") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => {
                let nfc = value
                    .as_object()
                    .ok_or_else(|| usage("nfc", "expected an object with `card_present`"))?;
                only(nfc, &["card_present"])?;
                Some(
                    opt_bool(nfc, "card_present")?
                        .ok_or_else(|| usage("nfc.card_present", "is required"))?,
                )
            }
        };
        let mic = match args.get("mic") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(MicArg::from_json(value)?),
        };
        let wifi = match args.get("wifi") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => Some(wifi_from_json(value)?),
        };
        let ble_bridge = match opt_str(args, "ble_bridge")? {
            None => None,
            Some("attach") => Some(true),
            Some("detach") => Some(false),
            Some(other) => {
                return Err(usage(
                    "ble_bridge",
                    &format!("`{other}` is not one of attach, detach"),
                ));
            }
        };
        let args = EnvArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            battery,
            usb,
            nfc_card: nfc,
            mic,
            wifi_aps: wifi,
            ble_bridge,
        };
        if args.is_empty() {
            return Err(ApiError::new(
                E_USAGE,
                "`env` needs at least one of `battery`, `usb`, `nfc`, `mic`, `wifi` or \
                 `ble_bridge`",
            )
            .with_hint("`status` reports the instance's state; `env` only changes it"));
        }
        Ok(args)
    }

    /// Naming nothing is a mistake rather than a no-op: it would still be journaled and cost a
    /// receipt.
    fn is_empty(&self) -> bool {
        self.battery.is_none()
            && self.usb.is_none()
            && self.nfc_card.is_none()
            && self.mic.is_none()
            && self.wifi_aps.is_none()
            && self.ble_bridge.is_none()
    }
}

/// `{ "aps": [ { ssid, bssid, rssi, channel, auth, psk } ] }`. `psk` is a secret input: journaled,
/// never echoed. An empty list is an air with no access point.
fn wifi_from_json(value: &serde_json::Value) -> Result<Vec<WifiAp>, ApiError> {
    let wifi = value
        .as_object()
        .ok_or_else(|| usage("wifi", "expected an object with `aps`"))?;
    only(wifi, &["aps"])?;
    let aps = wifi
        .get("aps")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| usage("wifi.aps", "expected an array of access points"))?;
    let mut out = Vec::with_capacity(aps.len());
    for (i, value) in aps.iter().enumerate() {
        let at = |field: &str| format!("wifi.aps[{i}].{field}");
        let ap = object(value)?;
        only(ap, &["ssid", "bssid", "rssi", "channel", "auth", "psk"])?;
        let bssid = req_str(ap, "bssid")?;
        let ap = WifiAp {
            ssid: req_str(ap, "ssid")?.to_owned(),
            bssid: parse_bssid(bssid).ok_or_else(|| {
                usage(
                    &at("bssid"),
                    "expected six hex octets like 02:00:00:47:32:01",
                )
            })?,
            rssi: i8::try_from(signed_in(ap, "rssi", -127, 0)?)
                .map_err(|_| usage(&at("rssi"), "is out of range"))?,
            channel: u8::try_from(int_in(ap, "channel", 1, u64::from(WIFI_MAX_CHANNEL))?)
                .map_err(|_| usage(&at("channel"), "is out of range"))?,
            authmode: u8::try_from(int_in(ap, "auth", 0, u64::from(WIFI_MAX_AUTH))?)
                .map_err(|_| usage(&at("auth"), "is out of range"))?,
            psk: opt_str(ap, "psk")?.unwrap_or_default().as_bytes().to_vec(),
        };
        // `WifiAp::check` carries the key rule and the field ranges, so this door and `pemu_input`
        // refuse the same keys. A key error names `psk`; the others name the access point.
        ap.check().map_err(|err| match err {
            WifiApError::Psk | WifiApError::PskStrength => usage(&at("psk"), err.detail()),
            other => usage(&format!("wifi.aps[{i}]"), other.detail()),
        })?;
        out.push(ap);
    }
    Ok(out)
}

/// Six octets from `02:00:00:47:32:01` form (the `docs/secrets.md` placeholder prefix).
pub(crate) fn parse_bssid(text: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = text.split(':');
    for slot in &mut out {
        let part = parts.next()?;
        if part.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

fn battery_from_json(value: &serde_json::Value) -> Result<BatterySet, ApiError> {
    let args = value
        .as_object()
        .ok_or_else(|| usage("battery", "expected an object"))?;
    only(args, &["mv", "soc", "present", "temp_c"])?;
    let mut set = BatterySet::default();
    if args.contains_key("mv") {
        set.mv = Some(
            u16::try_from(int_in(args, "mv", BATTERY_MV_MIN, BATTERY_MV_MAX)?)
                .map_err(|_| usage("battery.mv", "does not fit"))?,
        );
    }
    if args.contains_key("soc") {
        set.soc = Some(
            u8::try_from(int_in(args, "soc", 0, 100)?)
                .map_err(|_| usage("battery.soc", "does not fit"))?,
        );
    }
    if args.contains_key("temp_c") {
        let whole = signed_in(args, "temp_c", BATTERY_TEMP_C_MIN, BATTERY_TEMP_C_MAX)?;
        set.temp_c_deci = Some(
            i16::try_from(whole.saturating_mul(10))
                .map_err(|_| usage("battery.temp_c", "does not fit"))?,
        );
    }
    set.present = opt_bool(args, "present")?;
    if set == BatterySet::default() {
        return Err(usage(
            "battery",
            "expected at least one of mv, soc, present, temp_c",
        ));
    }
    Ok(set)
}

/// Non-negative, or `E_USAGE` naming the range.
fn int_in(args: &JsonMap, key: &str, low: u64, high: u64) -> Result<u64, ApiError> {
    let value = args
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| usage(key, &format!("expected an integer in {low}..={high}")))?;
    if value < low || value > high {
        return Err(usage(key, &format!("{value} is outside {low}..={high}")));
    }
    Ok(value)
}

fn signed_in(args: &JsonMap, key: &str, low: i64, high: i64) -> Result<i64, ApiError> {
    let value = args
        .get(key)
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| usage(key, &format!("expected an integer in {low}..={high}")))?;
    if value < low || value > high {
        return Err(usage(key, &format!("{value} is outside {low}..={high}")));
    }
    Ok(value)
}

/// `rail_on` comes from the caller's lifecycle: whether the MCU is powered decides between U1 and
/// U2, and a session carries no rail state of its own.
pub fn env_on(session: &mut Session, args: &EnvArgs, rail_on: bool) -> Result<Output, ApiError> {
    let mut applied = serde_json::Map::new();
    let mut effects: Vec<String> = Vec::new();
    let mut journaled = 0u32;

    if let Some(battery) = args.battery {
        journal(session, InputEvent::Battery(battery))?;
        journaled += 1;
        applied.insert("battery".into(), battery_json(&battery));
        effects.push("battery: the gauge answers the new values at the next read".to_owned());
    }
    if let Some(usb) = args.usb {
        for event in usb.events() {
            journal(session, event)?;
            journaled += 1;
        }
        applied.insert("usb".into(), usb.as_str().into());
        effects.push(usb.firmware_visible(rail_on).to_owned());
    }
    if let Some(present) = args.nfc_card {
        let ops = if present {
            vec![NfcOp::FieldOn]
        } else {
            vec![NfcOp::FieldOff]
        };
        journal(session, InputEvent::NfcTap { ops })?;
        journaled += 1;
        applied.insert("nfc".into(), serde_json::json!({ "card_present": present }));
        // The card has no connection to the MCU; only a reader conversation is firmware-visible,
        // and this instance is the card, not the reader.
        effects.push(
            "nfc: firmware-visible: none (the card talks to a reader, not to the MCU)".to_owned(),
        );
    }
    if let Some(mic) = &args.mic {
        journal(
            session,
            InputEvent::Env(EnvChange::MicSource(mic.source.clone())),
        )?;
        journaled += 1;
        applied.insert("mic".into(), mic.name().into());
        effects.push(
            match mic.source {
                // A live source is journaled as consumed, and a gap in its chunks makes the run
                // live rather than replayable.
                MicSource::Live => "mic: live capture makes the run non-deterministic",
                _ => "mic: the capture path reads the new source from the next sample on",
            }
            .to_owned(),
        );
    }

    if let Some(aps) = &args.wifi_aps {
        journal(session, InputEvent::Env(EnvChange::WifiAps(aps.clone())))?;
        journaled += 1;
        // The key is a secret input and is not echoed; the listing says whether each access point
        // has one.
        applied.insert(
            "wifi".into(),
            serde_json::json!({
                "aps": aps
                    .iter()
                    .map(|ap| serde_json::json!({
                        "ssid": ap.ssid,
                        "bssid": bssid_text(&ap.bssid),
                        "rssi": ap.rssi,
                        "channel": ap.channel,
                        "auth": ap.authmode,
                        "psk": if ap.psk.is_empty() { "none" } else { "set" },
                    }))
                    .collect::<Vec<_>>(),
            }),
        );
        effects.push(format!(
            "wifi: {} scripted access point(s); the next scan finds them",
            aps.len()
        ));
    }

    if let Some(attach) = args.ble_bridge {
        // The transport goes up before the journal entry and comes down after it, so the machine
        // never reports a bridge the host cannot carry, or carries one it does not know about.
        let io = super::ble_scan::installed_bridge_io();
        let url = match (attach, io) {
            (true, Some(io)) => Some((io.attach)(session).map_err(|err| {
                ApiError::new(E_STATE, format!("the HCI bridge could not be opened: {err}"))
                    .with_hint(
                        "a bridge listens on 127.0.0.1 and serves one peer at a time;                          `env --ble-bridge detach` closes the one that is open",
                    )
            })?),
            _ => None,
        };
        if let Err(error) = super::ble_scan::bridge_journal(session, attach) {
            if let (true, Some(io)) = (attach, super::ble_scan::installed_bridge_io()) {
                let _ = (io.detach)(session);
            }
            return Err(error);
        }
        journaled += 1;
        if !attach && let Some(io) = super::ble_scan::installed_bridge_io() {
            (io.detach)(session).map_err(|err| {
                ApiError::new(
                    E_STATE,
                    format!("the HCI bridge could not be closed: {err}"),
                )
            })?;
        }
        applied.insert(
            "ble_bridge".into(),
            serde_json::json!({
                "state": if attach { "attached" } else { "detached" },
                "url": url,
            }),
        );
        effects.push(match (attach, url.as_deref()) {
            (true, Some(url)) => format!(
                "ble: the external HCI bridge is up at {url}; the guest's packets wait for a peer                  there and the clock is held at realtime 1.000x"
            ),
            (true, None) => "ble: the external HCI bridge is up in the journal; this build opens                  no socket, so a peer reaches it only through a host that does"
                .to_owned(),
            (false, _) => {
                "ble: the external HCI bridge is down; the virtual controller answers the guest                  again and the clock is the caller's"
                    .to_owned()
            }
        });
    }

    let receipt = session.receipt();
    let notes = vec![
        format!("{journaled} journaled input(s) at vt={}us", receipt.vt_us),
        "applied at the next slice boundary; `run` is what lets the guest see it".to_owned(),
    ];
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "applied": serde_json::Value::Object(applied),
        "effects": effects,
        "notes": notes,
    });
    Ok(Output::new(json.clone(), render(&json), receipt).shaped(&ShapeLimits::DEFAULT))
}

/// The temperature in whole degrees again, so an agent reads back what it wrote.
fn battery_json(set: &BatterySet) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    if let Some(mv) = set.mv {
        map.insert("mv".into(), mv.into());
    }
    if let Some(soc) = set.soc {
        map.insert("soc".into(), soc.into());
    }
    if let Some(present) = set.present {
        map.insert("present".into(), present.into());
    }
    if let Some(deci) = set.temp_c_deci {
        map.insert("temp_c".into(), (i64::from(deci) / 10).into());
    }
    serde_json::Value::Object(map)
}

/// A refusal is `E_STATE`.
fn journal(session: &mut Session, event: InputEvent) -> Result<(), ApiError> {
    session
        .machine()
        .input(At::Now, event)
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the world change at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

/// One line naming what changed, then one per firmware-visible effect.
fn render(json: &serde_json::Value) -> String {
    let mut text = String::new();
    let applied = json["applied"].as_object().map(|map| {
        map.keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ")
    });
    let _ = writeln!(
        text,
        "{} env {} vt={}us",
        json["instance"].as_str().unwrap_or("?"),
        applied.as_deref().unwrap_or(""),
        json["vt_us"].as_u64().unwrap_or(0),
    );
    for effect in json["effects"].as_array().map_or(&[][..], Vec::as_slice) {
        let _ = writeln!(text, "  {}", effect.as_str().unwrap_or(""));
    }
    text.trim_end().to_owned()
}

pub(crate) fn bssid_text(bssid: &[u8; 6]) -> String {
    bssid
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn input_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`env` arguments; at least one part of the world is required.",
        "properties": {
            "instance": instance_schema(),
            "battery": {
                "type": "object",
                "additionalProperties": false,
                "description": "Fuel-gauge values; each field is optional on its own.",
                "properties": {
                    "mv": { "type": "integer", "minimum": BATTERY_MV_MIN, "maximum": BATTERY_MV_MAX },
                    "soc": { "type": "integer", "minimum": 0, "maximum": 100 },
                    "present": { "type": "boolean" },
                    "temp_c": { "type": "integer", "minimum": BATTERY_TEMP_C_MIN, "maximum": BATTERY_TEMP_C_MAX }
                }
            },
            "usb": { "type": "string", "enum": ["unplugged", "charger", "host", "open"], "description": "USB world state." },
            "nfc": {
                "type": "object",
                "additionalProperties": false,
                "required": ["card_present"],
                "description": "Whether a card sits in the reader field.",
                "properties": { "card_present": { "type": "boolean" } }
            },
            "mic": {
                "type": "object",
                "additionalProperties": false,
                "required": ["kind"],
                "description": "Microphone source.",
                "properties": {
                    "kind": { "type": "string", "enum": ["silence", "tone", "file", "live"] },
                    "hz": { "type": "integer", "minimum": 1, "maximum": MIC_TONE_HZ_MAX },
                    "amplitude": { "type": "integer", "minimum": 0, "maximum": 32767 },
                    "name": { "type": "string" }
                }
            },
            "ble_bridge": {
                "type": "string",
                "enum": ["attach", "detach"],
                "description": "The external HCI bridge: attach opens a loopback transport a peer connects to and diverts the guest's HCI packets to it; detach gives the guest back to the virtual controller. While it is attached the clock is held at realtime 1.000x."
            },
            "wifi": {
                "type": "object",
                "additionalProperties": false,
                "required": ["aps"],
                "description": "The scripted access points a Wi-Fi scan finds; replaces the set before.",
                "properties": {
                    "aps": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["ssid", "bssid", "rssi", "channel", "auth"],
                            "properties": {
                                "ssid": { "type": "string", "minLength": 1, "maxLength": WIFI_MAX_SSID },
                                "bssid": { "type": "string", "description": "Six hex octets; scripted ones start 02:00:00." },
                                "rssi": { "type": "integer", "minimum": -127, "maximum": 0 },
                                "channel": { "type": "integer", "minimum": 1, "maximum": WIFI_MAX_CHANNEL },
                                "auth": { "type": "integer", "minimum": 0, "maximum": WIFI_MAX_AUTH },
                                "psk": {
                                    "type": "string",
                                    "minLength": WIFI_MIN_PSK,
                                    "maxLength": WIFI_MAX_PSK,
                                    "description": "Secret input for a secured network: 6 to 64 bytes and not one repeated byte, so the secret set can hold it; never echoed back, and never written into a journal export."
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "applied": { "type": "object" },
            "effects": { "type": "array", "items": { "type": "string" } },
            "notes": { "type": "array", "items": { "type": "string" } }
        }
    })
}

/// Change the world around the device: battery, USB, NFC, microphone, Wi-Fi air, HCI bridge.
#[command(
    api_crate = crate,
    name = "env",
    group = core,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "env",
    errors(E_USAGE, E_STATE, E_LEASE, E_INTERNAL),
    example(
        title = "Set the cell to 3900 mV at 80 percent",
        args = r#"{"battery":{"mv":3900,"soc":80}}"#,
    ),
    example(
        title = "Unplug the USB cable",
        args = r#"{"usb":"unplugged"}"#,
    ),
    example(
        title = "Put a 440 Hz tone on the microphone",
        args = r#"{"mic":{"kind":"tone","hz":440,"amplitude":16384}}"#,
    ),
    example(
        title = "Take the NFC card out of the reader field",
        args = r#"{"nfc":{"card_present":false}}"#,
    ),
    example(
        title = "Attach the external HCI bridge, so a peer drives the controller",
        args = r#"{"ble_bridge":"attach"}"#,
    ),
)]
pub fn env(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = EnvArgs::from_json(&args)?;
    with_pool(|pool| env_on_pool(pool, &args))
}

/// Bind, check the lease, read the rail, journal.
pub fn env_on_pool(pool: &mut Pool, args: &EnvArgs) -> Result<Output, ApiError> {
    let id = pool.bind(SPEC_ENV.annotations, args.instance.as_deref())?;
    let now = pool
        .session(id)
        .map(Session::now)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    if let Some(state) = pool.table().get(id) {
        state
            .lease
            .check_call(crate::lease::LeaseHolder::Agent, SPEC_ENV.annotations, now)?;
    }
    let rail_on = pool
        .table()
        .get(id)
        .is_none_or(|state| state.lifecycle != Lifecycle::PoweredOff);
    let session = pool
        .session_mut(id)
        .ok_or_else(|| ApiError::new(E_STATE, format!("instance `{id}` has no session")))?;
    env_on(session, args, rail_on)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use pemu_core::hostio::HostIo;
    use pemu_core::time::VTime;
    use pemu_machine::MachineApi;
    use pemu_machine::machine::{GuestMem, InputError, Receipt as LedgerReceipt};
    use pemu_machine::run::{RunLimits, RunOutcome};
    use pemu_machine::stops::StopReason;

    use crate::commands::start::{Boot, StartArgs};
    use crate::instance::InstanceId;

    /// `pemu-api` may not dev-depend on `pemu-testkit`, and everything `env` promises is which
    /// `InputEvent`s it handed the machine, in which order.
    pub(crate) struct JournalMachine {
        vt: VTime,
        io: HostIo,
        journal: Arc<Mutex<Vec<InputEvent>>>,
    }

    impl JournalMachine {
        pub(crate) fn new() -> (JournalMachine, Arc<Mutex<Vec<InputEvent>>>) {
            let journal = Arc::new(Mutex::new(Vec::new()));
            let machine = JournalMachine {
                vt: VTime(0),
                io: HostIo::new(1024),
                journal: Arc::clone(&journal),
            };
            (machine, journal)
        }
    }

    crate::commands::start::tests::refuse_snapshots!(JournalMachine);

    impl MachineApi for JournalMachine {
        fn run(&mut self, lim: RunLimits) -> RunOutcome {
            if let Some(until) = lim.until {
                self.vt = VTime(self.vt.0.max(until.0));
            }
            RunOutcome {
                reason: StopReason::Until,
                vt: self.vt,
                insns: 0,
                ff_insns: 0,
                idle_ps: 0,
            }
        }

        fn input(&mut self, _at: At, ev: InputEvent) -> Result<u64, InputError> {
            let mut journal = self.journal.lock().expect("the journal is never poisoned");
            journal.push(ev);
            Ok(journal.len() as u64 - 1)
        }

        fn io(&mut self) -> &mut HostIo {
            &mut self.io
        }

        fn now(&self) -> VTime {
            self.vt
        }

        fn guest_mem(&mut self) -> GuestMem<'_> {
            unreachable!("`env` reads no guest memory")
        }

        fn is_tainted(&self) -> bool {
            false
        }
        fn receipt(&mut self) -> LedgerReceipt {
            LedgerReceipt::default()
        }
    }

    pub(crate) fn journaled() -> (Pool, InstanceId, Arc<Mutex<Vec<InputEvent>>>) {
        let (machine, journal) = JournalMachine::new();
        let mut pool = Pool::new();
        let args = StartArgs {
            fw: "official".to_owned(),
            boot: Boot::None,
            ..StartArgs::default()
        };
        let id = pool.attach(&args, Box::new(machine));
        pool.table_mut()
            .get_mut(id)
            .expect("the instance was just created")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("starting -> paused");
        (pool, id, journal)
    }

    fn parse(json: serde_json::Value) -> Result<EnvArgs, ApiError> {
        EnvArgs::from_json(&json)
    }

    fn events(journal: &Arc<Mutex<Vec<InputEvent>>>) -> Vec<InputEvent> {
        journal.lock().expect("not poisoned").clone()
    }

    #[test]
    fn a_battery_set_becomes_one_journaled_battery_event() {
        let (mut pool, _, journal) = journaled();
        let args = parse(serde_json::json!({"battery":{"mv":3900,"soc":80,"present":true}}))
            .expect("inside the schema");
        let out = env_on_pool(&mut pool, &args).expect("the world can always be set");
        assert_eq!(out.json["applied"]["battery"]["mv"], 3900);
        assert_eq!(out.json["applied"]["battery"]["soc"], 80);
        assert_eq!(
            events(&journal),
            vec![InputEvent::Battery(BatterySet {
                soc: Some(80),
                mv: Some(3900),
                temp_c_deci: None,
                present: Some(true),
            })]
        );
    }

    /// The argument is whole degrees; `BatterySet` keeps tenths so no float reaches the guest.
    #[test]
    fn a_temperature_round_trips_through_the_deci_degree_field() {
        let args = parse(serde_json::json!({"battery":{"temp_c":-15}})).expect("inside the range");
        assert_eq!(args.battery.expect("a battery set").temp_c_deci, Some(-150));
        let (mut pool, _, _) = journaled();
        let out = env_on_pool(&mut pool, &args).expect("the world can always be set");
        assert_eq!(out.json["applied"]["battery"]["temp_c"], -15);
    }

    #[test]
    fn each_usb_word_journals_the_cable_and_the_client() {
        for (word, cable, client) in [
            ("unplugged", false, false),
            ("charger", true, false),
            ("host", true, false),
            ("open", true, true),
        ] {
            let (mut pool, _, journal) = journaled();
            let args = parse(serde_json::json!({ "usb": word })).expect("inside the vocabulary");
            env_on_pool(&mut pool, &args).expect("the world can always be set");
            assert_eq!(
                events(&journal),
                vec![
                    InputEvent::UsbCable { plugged: cable },
                    InputEvent::UsbClient { open: client },
                ],
                "usb {word}"
            );
        }
    }

    /// The firmware has no VBUS signal.
    #[test]
    fn the_effects_say_the_firmware_cannot_tell_a_charger_from_an_idle_host() {
        let (mut pool, _, _) = journaled();
        let charger = env_on_pool(
            &mut pool,
            &parse(serde_json::json!({"usb":"charger"})).expect("a word"),
        )
        .expect("set");
        let host = env_on_pool(
            &mut pool,
            &parse(serde_json::json!({"usb":"host"})).expect("a word"),
        )
        .expect("set");
        assert_eq!(charger.json["effects"], host.json["effects"]);
        assert!(
            charger.json["effects"][0]
                .as_str()
                .expect("one effect")
                .contains("U2 ATTACHED_IDLE"),
            "{}",
            charger.json["effects"][0]
        );
    }

    /// `unplugged` is separable because U0 stops SOF and freezes `FRAME_NUM`.
    #[test]
    fn the_pair_the_firmware_cannot_tell_apart_is_charger_and_host() {
        // Keeps the module header's claim in step.
        assert_eq!(
            UsbWorld::Charger.events(),
            UsbWorld::Host.events(),
            "`charger` and `host` journal the same thing, which is why the guest cannot \
             separate them"
        );
        assert_eq!(
            UsbWorld::Charger.firmware_visible(true),
            UsbWorld::Host.firmware_visible(true)
        );
        assert_ne!(
            UsbWorld::Charger.events(),
            UsbWorld::Unplugged.events(),
            "`unplugged` is U0 and `charger` is U2; SOF runs only in U2 and U3, so the \
             firmware can tell these two apart"
        );
        assert_ne!(
            UsbWorld::Charger.firmware_visible(true),
            UsbWorld::Unplugged.firmware_visible(true)
        );
    }

    #[test]
    fn an_nfc_card_change_reports_no_firmware_visible_effect() {
        let (mut pool, _, journal) = journaled();
        let args = parse(serde_json::json!({"nfc":{"card_present":true}})).expect("inside");
        let out = env_on_pool(&mut pool, &args).expect("set");
        assert!(
            out.json["effects"][0]
                .as_str()
                .expect("one effect")
                .contains("firmware-visible: none"),
            "{}",
            out.json["effects"][0]
        );
        assert_eq!(
            events(&journal),
            vec![InputEvent::NfcTap {
                ops: vec![NfcOp::FieldOn]
            }]
        );
    }

    #[test]
    fn a_mic_source_is_journaled_as_an_env_change() {
        let (mut pool, _, journal) = journaled();
        let args = parse(serde_json::json!({"mic":{"kind":"tone","hz":440,"amplitude":16384}}))
            .expect("inside the schema");
        env_on_pool(&mut pool, &args).expect("set");
        assert_eq!(
            events(&journal),
            vec![InputEvent::Env(EnvChange::MicSource(MicSource::Tone {
                hz: 440,
                amplitude: 16384
            }))]
        );
    }

    #[test]
    fn a_live_mic_source_says_it_makes_the_run_non_deterministic() {
        let (mut pool, _, _) = journaled();
        let args = parse(serde_json::json!({"mic":{"kind":"live"}})).expect("inside the schema");
        let out = env_on_pool(&mut pool, &args).expect("set");
        assert!(
            out.json["effects"][0]
                .as_str()
                .expect("one effect")
                .contains("non-deterministic"),
            "{}",
            out.json["effects"][0]
        );
    }

    /// A journaled input is applied at the next slice boundary.
    #[test]
    fn env_does_not_advance_virtual_time_and_says_when_the_guest_sees_the_change() {
        let (mut pool, id, _) = journaled();
        let before = pool.session(id).expect("the instance").now();
        let out = env_on_pool(
            &mut pool,
            &parse(serde_json::json!({"battery":{"soc":50}})).expect("inside the schema"),
        )
        .expect("set");
        assert_eq!(pool.session(id).expect("the instance").now(), before);
        assert_eq!(out.json["vt_us"], before.as_us());
        assert!(
            out.json["notes"][1]
                .as_str()
                .expect("a note")
                .contains("next slice boundary"),
            "{}",
            out.json["notes"][1]
        );
        const { assert!(!SPEC_ENV.annotations.advances_time) };
    }

    /// The order a replay has to reproduce.
    #[test]
    fn several_parts_of_the_world_are_journaled_in_a_fixed_order() {
        let (mut pool, _, journal) = journaled();
        let args = parse(serde_json::json!({
            "battery": {"soc": 10},
            "usb": "open",
            "nfc": {"card_present": false},
            "mic": {"kind": "silence"}
        }))
        .expect("inside the schema");
        env_on_pool(&mut pool, &args).expect("set");
        let kinds: Vec<&str> = events(&journal)
            .iter()
            .map(|event| match event {
                InputEvent::Battery(_) => "battery",
                InputEvent::UsbCable { .. } => "cable",
                InputEvent::UsbClient { .. } => "client",
                InputEvent::NfcTap { .. } => "nfc",
                InputEvent::Env(EnvChange::MicSource(_)) => "mic",
                InputEvent::Env(EnvChange::WifiAps(_)) => "wifi",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["battery", "cable", "client", "nfc", "mic"],
            "the documented order of `env_on`"
        );
    }

    /// Never in the output, taints the instance so `snapshot export` refuses, and a fork is tainted
    /// too.
    #[test]
    fn a_scripted_key_taints_the_instance_and_is_never_echoed() {
        use crate::commands::snapshot::tests::args;
        use crate::commands::snapshot::{snapshot_on_pool, tests::real_instance, tests::world};
        use crate::error::E_SECRET_REFUSED;

        const KEY: &str = "scripted-key-123";
        let _world = world();
        let (mut pool, _) = real_instance();
        let out = env_on_pool(
            &mut pool,
            &parse(serde_json::json!({ "wifi": { "aps": [
                { "ssid": "G2-Alpha", "bssid": "02:00:00:47:32:01", "rssi": -42, "channel": 1,
                  "auth": 3, "psk": KEY }
            ] } }))
            .expect("inside the schema"),
        )
        .expect("the air is scripted");
        let rendered = format!("{} {}", out.json, out.text);
        assert!(!rendered.contains(KEY), "the key is not echoed: {rendered}");

        let error = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"export","name":"wifi"})),
        )
        .expect_err("a scripted key taints the instance");
        assert_eq!(error.code, E_SECRET_REFUSED);

        // A fork carries the journal and the world.
        let out = snapshot_on_pool(
            &mut pool,
            &args(serde_json::json!({"op":"fork","name":"f","count":1})),
        )
        .expect("fork");
        let fork =
            crate::instance::InstanceId::parse(out.json["instances"][0].as_str().expect("an id"))
                .expect("a minted id");
        crate::commands::snapshot::secrets_of(pool.session_mut(fork).expect("the fork"));
        assert!(pool.with_store(|store| store.is_tainted(fork)));
    }

    /// Refused rather than journaled in the clear, as `nfc_tap` refuses a WSC network key.
    #[test]
    fn a_key_the_secret_set_cannot_hold_is_refused() {
        let with_key = |psk: String| {
            serde_json::json!({ "wifi": { "aps": [
                { "ssid": "G2-Alpha", "bssid": "02:00:00:47:32:01", "rssi": -42, "channel": 1,
                  "auth": 3, "psk": psk }
            ] } })
        };
        for psk in ["abc", "aaaaaaaa", &"k".repeat(65)] {
            let err = parse(with_key(psk.to_string())).expect_err("refused");
            assert_eq!(err.code, crate::error::E_USAGE, "psk `{psk}`");
            assert!(
                !format!("{err:?}").contains(psk),
                "the refusal does not echo the key"
            );
        }
        assert!(
            parse(with_key("scripted-key".to_string())).is_ok(),
            "a key the set can hold is taken"
        );
    }

    #[test]
    fn scripted_access_points_are_journaled_and_their_keys_are_never_echoed() {
        let (mut pool, _, journal) = journaled();
        let args = parse(serde_json::json!({
            "wifi": { "aps": [
                { "ssid": "G2-Alpha", "bssid": "02:00:00:47:32:01", "rssi": -42, "channel": 1,
                  "auth": 3, "psk": "scripted-key" },
                { "ssid": "G2-Bravo", "bssid": "02:00:00:47:32:02", "rssi": -60, "channel": 6,
                  "auth": 0 }
            ] }
        }))
        .expect("inside the schema");
        let out = env_on_pool(&mut pool, &args).expect("set");
        let text = format!("{}{}", out.text, out.json);
        assert!(
            !text.contains("scripted-key"),
            "the key is not echoed: {text}"
        );
        assert!(text.contains("G2-Alpha") && text.contains("02:00:00:47:32:01"));
        let events = events(&journal);
        assert_eq!(events.len(), 1, "one journaled input");
        let InputEvent::Env(EnvChange::WifiAps(aps)) = &events[0] else {
            panic!("expected the scripted access points, got {:?}", events[0]);
        };
        assert_eq!(aps.len(), 2);
        assert_eq!(aps[0].psk, b"scripted-key");
        assert_eq!(aps[0].bssid, [0x02, 0x00, 0x00, 0x47, 0x32, 0x01]);
        assert!(aps[1].psk.is_empty(), "an open network carries no key");
    }

    /// Per `pemu_core::input::WifiAp::check`.
    #[test]
    fn a_scripted_access_point_outside_the_schema_is_usage() {
        let ap = |extra: serde_json::Value| {
            let mut ap = serde_json::json!({
                "ssid": "G2-Alpha", "bssid": "02:00:00:47:32:01", "rssi": -42,
                "channel": 1, "auth": 0
            });
            for (k, v) in extra.as_object().expect("object") {
                ap[k] = v.clone();
            }
            serde_json::json!({ "wifi": { "aps": [ap] } })
        };
        for bad in [
            ap(serde_json::json!({ "channel": 12 })),
            ap(serde_json::json!({ "auth": 11 })),
            ap(serde_json::json!({ "ssid": "" })),
            ap(serde_json::json!({ "psk": "key" })),
            ap(serde_json::json!({ "bssid": "02:00:00:47:32" })),
            ap(serde_json::json!({ "nonsense": 1 })),
            serde_json::json!({ "wifi": {} }),
            serde_json::json!({ "wifi": { "aps": 1 } }),
        ] {
            let err = parse(bad.clone()).expect_err("refused");
            assert_eq!(err.code, E_USAGE, "{bad}");
        }
        // A secured network with a key, and an empty air, are both accepted.
        assert!(parse(ap(serde_json::json!({ "auth": 3, "psk": "scripted-key" }))).is_ok());
        assert!(parse(serde_json::json!({ "wifi": { "aps": [] } })).is_ok());
    }

    #[test]
    fn every_argument_outside_the_schema_is_usage() {
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "nonsense": 1 }),
            serde_json::json!({ "usb": "attached" }),
            serde_json::json!({ "battery": {} }),
            serde_json::json!({ "battery": { "mv": 2400 } }),
            serde_json::json!({ "battery": { "mv": 4401 } }),
            serde_json::json!({ "battery": { "soc": 101 } }),
            serde_json::json!({ "battery": { "temp_c": 81 } }),
            serde_json::json!({ "battery": { "ramp": {} } }),
            serde_json::json!({ "nfc": {} }),
            serde_json::json!({ "mic": { "kind": "radio" } }),
            serde_json::json!({ "mic": { "kind": "tone" } }),
            serde_json::json!({ "mic": { "kind": "tone", "hz": 9000, "amplitude": 1 } }),
            serde_json::json!({ "mic": { "kind": "file" } }),
        ] {
            assert_eq!(
                parse(bad.clone()).expect_err("outside the schema").code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    #[test]
    fn every_registered_example_parses_as_its_own_arguments() {
        let spec = crate::registry::find("env").expect("#[command] registered env");
        for example in spec.examples {
            let json = example.args_json().expect("an example is JSON");
            EnvArgs::from_json(&json).unwrap_or_else(|e| panic!("{}: {e:?}", example.title));
        }
        assert!(spec.annotations.needs_instance);
        assert!(!spec.annotations.read_only, "`env` changes the world");
        assert_eq!(spec.scenario_step, Some("env"));
    }
}
