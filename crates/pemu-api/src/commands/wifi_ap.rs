//! `passportsim wifi_ap`: add, update, remove or list one scripted access point, the `radio` caps
//! group's spelling of what `env`'s `wifi` section scripts whole.
//!
//! The machine takes the air as a whole (`EnvChange::WifiAps`), so this command reads the air the
//! module holds, changes the one access point it names (by `ssid`, and `bssid` when given; an
//! unknown one is added) and journals the result. The module keeps the other keys, so the new world
//! carries them without this command holding them. With no `ssid` the call journals nothing and
//! lists the air.
//!
//! `key` is a secret input checked by `WifiAp::check`, as at every door: it must be one the
//! `SecretSet` can hold, so it masks in every output and taints the instance. It is never echoed;
//! the listing says `key: set` or `key: none`. SSID and BSSID are the caller's own and never
//! redacted.

use pemu_core::input::{
    EnvChange, InputEvent, WIFI_MAX_AUTH, WIFI_MAX_CHANNEL, WifiAp, WifiApError,
};
use pemu_machine::machine::At;

use crate::error::{ApiError, E_INTERNAL, E_LEASE, E_STATE, E_USAGE};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::env::{bssid_text, parse_bssid};
use super::net_http::wifi_state;
use super::nfc_tap::bind_checked;
use crate::args::{instance_schema, object, only, opt_bool, opt_str, usage};
use crate::session::Session;

/// Class C, the web card's default.
pub const DEFAULT_RSSI: i8 = -55;
pub const DEFAULT_CHANNEL: u8 = 1;

/// `wifi_auth_mode_t` (ESP-IDF `esp_wifi_types_generic.h`); a number 0 to 10 is taken as well.
pub const AUTH_NAMES: [(&str, u8); 8] = [
    ("open", 0),
    ("wep", 1),
    ("wpa-psk", 2),
    ("wpa2-psk", 3),
    ("wpa-wpa2-psk", 4),
    ("wpa2-enterprise", 5),
    ("wpa3-psk", 6),
    ("wpa2-wpa3-psk", 7),
];

fn auth_name(mode: u8) -> String {
    AUTH_NAMES
        .iter()
        .find(|(_, m)| *m == mode)
        .map_or_else(|| mode.to_string(), |(n, _)| (*n).to_owned())
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct WifiApArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    /// `None` to list.
    pub ssid: Option<String>,
    pub bssid: Option<[u8; 6]>,
    /// In dBm.
    pub rssi: Option<i8>,
    pub channel: Option<u8>,
    /// `wifi_auth_mode_t`.
    pub auth: Option<u8>,
    /// A secret input.
    pub key: Option<Vec<u8>>,
    pub remove: bool,
}

impl WifiApArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<WifiApArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance", "ssid", "bssid", "rssi", "channel", "auth", "key", "remove",
            ],
        )?;
        let bssid =
            match opt_str(args, "bssid")? {
                None => None,
                Some(text) => Some(parse_bssid(text).ok_or_else(|| {
                    usage("bssid", "expected six hex octets like 02:00:00:47:32:01")
                })?),
            };
        let rssi = match args.get("rssi").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(
                v.as_i64()
                    .filter(|r| (-127..=0).contains(r))
                    .and_then(|r| i8::try_from(r).ok())
                    .ok_or_else(|| usage("rssi", "expected a whole dBm in -127..=0"))?,
            ),
        };
        let channel = match args.get("channel").filter(|v| !v.is_null()) {
            None => None,
            Some(v) => Some(
                v.as_u64()
                    .filter(|c| (1..=u64::from(WIFI_MAX_CHANNEL)).contains(c))
                    .and_then(|c| u8::try_from(c).ok())
                    .ok_or_else(|| {
                        usage(
                            "channel",
                            &format!("expected a channel in 1..={WIFI_MAX_CHANNEL}"),
                        )
                    })?,
            ),
        };
        let auth = match args.get("auth").filter(|v| !v.is_null()) {
            None => None,
            Some(serde_json::Value::String(name)) => Some(
                AUTH_NAMES
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, m)| *m)
                    .ok_or_else(|| {
                        usage(
                            "auth",
                            &format!(
                                "`{name}` is not one of {}",
                                AUTH_NAMES.map(|(n, _)| n).join(", ")
                            ),
                        )
                    })?,
            ),
            Some(v) => Some(
                v.as_u64()
                    .filter(|m| *m <= u64::from(WIFI_MAX_AUTH))
                    .and_then(|m| u8::try_from(m).ok())
                    .ok_or_else(|| {
                        usage(
                            "auth",
                            &format!("expected a name or a number in 0..={WIFI_MAX_AUTH}"),
                        )
                    })?,
            ),
        };
        let ssid = opt_str(args, "ssid")?.map(str::to_owned);
        let remove = opt_bool(args, "remove")?.unwrap_or(false);
        let key = opt_str(args, "key")?.map(|k| k.as_bytes().to_vec());
        let named = bssid.is_some()
            || rssi.is_some()
            || channel.is_some()
            || auth.is_some()
            || key.is_some()
            || remove;
        if ssid.is_none() && named {
            return Err(usage("ssid", "names the access point the call changes"));
        }
        if remove && (rssi.is_some() || channel.is_some() || auth.is_some() || key.is_some()) {
            return Err(usage(
                "remove",
                "takes only the `ssid` (and `bssid`) of the access point",
            ));
        }
        Ok(WifiApArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            ssid,
            bssid,
            rssi,
            channel,
            auth,
            key,
            remove,
        })
    }

    fn names(&self, ap: &WifiAp) -> bool {
        self.ssid.as_deref() == Some(ap.ssid.as_str()) && self.bssid.is_none_or(|b| b == ap.bssid)
    }
}

/// For an access point added without one: the `02:00:00` placeholder prefix and three bytes of an
/// FNV-1a hash of the SSID, so every host gives the same address.
pub fn derived_bssid(ssid: &str) -> [u8; 6] {
    let mut h: u32 = 0x811C_9DC5;
    for b in ssid.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    let b = h.to_le_bytes();
    [0x02, 0x00, 0x00, b[0], b[1], b[2]]
}

/// Also what the change was.
pub fn apply(air: &[WifiAp], args: &WifiApArgs) -> Result<(Vec<WifiAp>, &'static str), ApiError> {
    let Some(ssid) = &args.ssid else {
        return Ok((air.to_vec(), "listed"));
    };
    let mut out = air.to_vec();
    let at = out.iter().position(|ap| args.names(ap));
    if args.remove {
        let Some(at) = at else {
            return Err(ApiError::new(
                E_STATE,
                format!("no scripted access point `{ssid}` is on the air"),
            )
            .with_hint("`wifi_ap` with no arguments lists the air"));
        };
        out.remove(at);
        return Ok((out, "removed"));
    }
    let (mut ap, what) = match at {
        Some(at) => (out.remove(at), "updated"),
        None => (
            WifiAp {
                ssid: ssid.clone(),
                bssid: args.bssid.unwrap_or_else(|| derived_bssid(ssid)),
                rssi: DEFAULT_RSSI,
                channel: DEFAULT_CHANNEL,
                authmode: 0,
                psk: Vec::new(),
            },
            "added",
        ),
    };
    if let Some(rssi) = args.rssi {
        ap.rssi = rssi;
    }
    if let Some(channel) = args.channel {
        ap.channel = channel;
    }
    if let Some(key) = &args.key {
        ap.psk = key.clone();
        if args.auth.is_none() && ap.authmode == 0 {
            // A key with no mode named is the pair the web card sends: WPA2-PSK.
            ap.authmode = 3;
        }
    }
    if let Some(auth) = args.auth {
        ap.authmode = auth;
        if auth == 0 {
            ap.psk.clear();
        }
    }
    ap.check().map_err(|err| match err {
        WifiApError::Psk | WifiApError::PskStrength => usage("key", err.detail()),
        other => usage("ssid", other.detail()),
    })?;
    match at {
        Some(at) => out.insert(at, ap),
        None => out.push(ap),
    }
    Ok((out, what))
}

/// The key is never echoed.
pub fn ap_json(ap: &WifiAp) -> serde_json::Value {
    serde_json::json!({
        "ssid": ap.ssid,
        "bssid": bssid_text(&ap.bssid),
        "rssi": ap.rssi,
        "channel": ap.channel,
        "auth": auth_name(ap.authmode),
        "key": if ap.psk.is_empty() { "none" } else { "set" },
    })
}

fn no_module() -> ApiError {
    ApiError::new(
        E_STATE,
        "this instance has no bound Wi-Fi module, so the change reached no air",
    )
    .with_hint(
        "`inspect fidelity` reports the binding; the change is in the journal and applies if \
         the module binds after a restore",
    )
}

pub fn wifi_ap_on(session: &mut Session, args: &WifiApArgs) -> Result<Output, ApiError> {
    // Anything journaled before this call applies first, so the air read here is the air now.
    let now = session.now();
    session.run_until(now);
    let air = wifi_state(session)?.map(|st| st.aps).unwrap_or_default();
    let (next, what) = apply(&air, args)?;
    if args.ssid.is_some() {
        session
            .machine()
            .input(At::Now, InputEvent::Env(EnvChange::WifiAps(next.clone())))
            .map_err(|_| {
                ApiError::new(
                    E_STATE,
                    "the machine refused the scripted air at the current instant",
                )
            })?;
        let now = session.now();
        session.run_until(now);
        if wifi_state(session)?.is_none() {
            return Err(no_module());
        }
    }
    let receipt = session.receipt();
    let aps: Vec<serde_json::Value> = next.iter().map(ap_json).collect();
    let mut text = format!(
        "{} wifi_ap {what}: {} access point(s) on the air",
        session.id,
        next.len()
    );
    for ap in &next {
        text.push_str(&format!(
            "\n  {} {} ch{} {} dBm {} key={}",
            ap.ssid,
            bssid_text(&ap.bssid),
            ap.channel,
            ap.rssi,
            auth_name(ap.authmode),
            if ap.psk.is_empty() { "none" } else { "set" }
        ));
    }
    let json = serde_json::json!({
        "instance": session.id.to_string(),
        "vt_us": receipt.vt_us,
        "change": what,
        "aps": aps,
    });
    Ok(Output::new(json, text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// The web contract `WifiApArgs`.
pub fn input_schema() -> Schema {
    let schema = serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "description": "`wifi_ap` arguments; none lists the air.",
        "properties": {
            "instance": instance_schema(),
            "ssid": { "type": "string", "minLength": 1, "maxLength": 32 },
            "bssid": { "type": "string", "pattern": "^([0-9a-fA-F]{2}:){5}[0-9a-fA-F]{2}$" },
            "rssi": { "type": "integer", "minimum": -127, "maximum": 0 },
            "channel": { "type": "integer", "minimum": 1, "maximum": WIFI_MAX_CHANNEL },
            "auth": {
                "oneOf": [
                    { "enum": AUTH_NAMES.map(|(n, _)| n) },
                    { "type": "integer", "minimum": 0, "maximum": WIFI_MAX_AUTH }
                ]
            },
            "key": { "type": "string", "minLength": 6, "maxLength": 64, "description": "Secret; never echoed." },
            "remove": { "type": "boolean" }
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
            "change": { "enum": ["listed", "added", "updated", "removed"] },
            "aps": { "type": "array", "items": { "type": "object" } }
        }
    })
}

/// Add, update, remove or list one scripted Wi-Fi access point.
#[command(
    api_crate = crate,
    name = "wifi_ap",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(needs_instance),
    scenario_step = "wifi.ap",
    errors(E_USAGE, E_STATE, E_LEASE, E_INTERNAL),
    example(
        title = "Put an open access point on channel 6",
        args = r#"{"ssid":"passport-emu-virtual-ap","channel":6,"rssi":-40}"#,
    ),
    example(
        title = "List the air",
        args = r#"{}"#,
    ),
    example(
        title = "Take it off the air",
        args = r#"{"ssid":"passport-emu-virtual-ap","remove":true}"#,
    ),
)]
pub fn wifi_ap(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = WifiApArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_WIFI_AP.annotations, args.instance.as_deref()),
        |session| wifi_ap_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: serde_json::Value) -> WifiApArgs {
        WifiApArgs::from_json(&v).expect("parses")
    }

    #[test]
    fn an_access_point_is_added_updated_and_removed_by_name() {
        let (air, what) = apply(&[], &parse(serde_json::json!({"ssid":"lab"}))).unwrap();
        assert_eq!(what, "added");
        assert_eq!(air.len(), 1);
        assert_eq!(
            &air[0].bssid[..3],
            &[0x02, 0x00, 0x00],
            "a placeholder BSSID"
        );
        assert_eq!(air[0].bssid, derived_bssid("lab"), "deterministic");
        assert_eq!((air[0].rssi, air[0].channel, air[0].authmode), (-55, 1, 0));

        let (air, what) = apply(
            &air,
            &parse(serde_json::json!({"ssid":"lab","channel":11,"key":"lab-passphrase"})),
        )
        .unwrap();
        assert_eq!(what, "updated");
        assert_eq!(air.len(), 1);
        assert_eq!(
            (air[0].channel, air[0].authmode),
            (11, 3),
            "a key makes it WPA2-PSK"
        );
        assert_eq!(air[0].psk, b"lab-passphrase");

        let (air, _) = apply(
            &air,
            &parse(serde_json::json!({"ssid":"other","auth":"open"})),
        )
        .unwrap();
        assert_eq!(air.len(), 2);
        assert_eq!(air[0].psk, b"lab-passphrase", "another AP's key is kept");
        let (air, what) = apply(
            &air,
            &parse(serde_json::json!({"ssid":"lab","remove":true})),
        )
        .unwrap();
        assert_eq!(what, "removed");
        assert_eq!(air.len(), 1);
        let err = apply(
            &air,
            &parse(serde_json::json!({"ssid":"lab","remove":true})),
        )
        .expect_err("not on the air");
        assert_eq!(err.code, E_STATE);
    }

    #[test]
    fn a_key_the_secret_set_cannot_hold_is_refused_at_this_door() {
        for bad in [
            serde_json::json!({"ssid":"lab","key":"short"}),
            serde_json::json!({"ssid":"lab","key":"aaaaaaaa"}),
            serde_json::json!({"ssid":"lab","auth":"wpa2-psk"}),
        ] {
            let err = apply(&[], &parse(bad.clone())).expect_err("refused");
            assert_eq!(err.code, E_USAGE, "{bad}");
        }
    }

    #[test]
    fn the_arguments_refuse_by_name_and_the_listing_never_echoes_a_key() {
        for bad in [
            serde_json::json!({"channel":6}),
            serde_json::json!({"ssid":"x","channel":12}),
            serde_json::json!({"ssid":"x","rssi":5}),
            serde_json::json!({"ssid":"x","auth":"wpa9"}),
            serde_json::json!({"ssid":"x","bssid":"zz"}),
            serde_json::json!({"ssid":"x","remove":true,"channel":3}),
            serde_json::json!({"ssid":"x","psk":"lab-passphrase"}),
        ] {
            let err = WifiApArgs::from_json(&bad).expect_err("refused");
            assert_eq!(err.code, E_USAGE, "{bad}");
        }
        let (air, _) = apply(
            &[],
            &parse(serde_json::json!({"ssid":"lab","key":"lab-passphrase"})),
        )
        .unwrap();
        let shown = ap_json(&air[0]).to_string();
        assert!(!shown.contains("lab-passphrase"), "{shown}");
        assert!(shown.contains("\"key\":\"set\""), "{shown}");
    }

    #[test]
    fn every_example_parses_and_the_command_is_in_the_radio_group() {
        let spec = crate::registry::find("wifi_ap").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let value = example.args_json().expect("the example is JSON");
            WifiApArgs::from_json(&value).expect("every example parses");
        }
    }
}
