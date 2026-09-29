//! `passportsim ble_gatt`: the GATT client of the scripted central, what was notified, and the
//! btsnoop capture as an artifact. The half shared with `ble_scan` and `ble_connect` is in
//! `ble_scan.rs`.
//!
//! | `op` | journals | answers with |
//! |---|---|---|
//! | `discover` | `DiscoverServices`, then one `DiscoverCharacteristics` per service found | the service tree |
//! | `read` | `Read` | the attribute value |
//! | `write` | `Write` (`ATT_WRITE_REQ`, or `ATT_WRITE_CMD` without `with_response`) | the step result |
//! | `subscribe` | `Subscribe` (a CCCD write, notifications or `indicate`) | the CCCD handle |
//! | `notifications` | `WaitNotification` when `contains` is given, nothing otherwise | what was notified |
//! | `capture` | `Capture` when `enabled` is given, nothing otherwise | the btsnoop artifact |
//!
//! `discover` is two scripts because characteristics are discovered per service (Core Vol 3 Part G
//! 4.6.1). A characteristic is named by `uuid` or by a `handle` a `discover` reported.
//! `notifications` with `contains` waits for those bytes; with `settle_ms` it reports what arrived
//! in that much virtual time; with neither, what already arrived.
//!
//! `capture` writes the btsnoop file after the value redaction pass. `Capture::record` zeroes LTKs,
//! IRKs and SMP values; `secrets: true` turns that off from then on (a journaled mode) and taints
//! the instance, so `snapshot export` then needs confirmation.

use pemu_core::time::VTime;
use pemu_radio::ble::btsnoop;
use pemu_radio::ble::central::{Characteristic, Notification, Service, Step, Uuid};

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TIMEOUT,
    E_TRIPWIRE, E_USAGE, E_WALL_BUDGET,
};
use crate::output::{ArtifactRef, Output};
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::ble_scan::{
    Ran, ble_state, common_json, common_properties, hex_text, not_connected, not_running,
    parse_hex, parse_uuid, run_steps, step_refused, timeout_of, uuid_text, wall_budget_of,
};
use super::nfc_tap::bind_checked;
use crate::args::{object, only, opt_bool, opt_str, opt_u64, usage};
use crate::session::Session;

pub const BLE_DIR: &str = "ble";
/// No registered type exists for the format, so it is an opaque byte stream.
pub const BTSNOOP_MEDIA_TYPE: &str = "application/octet-stream";
pub const DEFAULT_WITHIN_MS: u64 = 5_000;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Discover,
    Read,
    Write,
    Subscribe,
    Notifications,
    Capture,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Discover => "discover",
            Op::Read => "read",
            Op::Write => "write",
            Op::Subscribe => "subscribe",
            Op::Notifications => "notifications",
            Op::Capture => "capture",
        }
    }

    pub fn parse(text: &str) -> Option<Op> {
        Some(match text {
            "discover" => Op::Discover,
            "read" => Op::Read,
            "write" => Op::Write,
            "subscribe" => Op::Subscribe,
            "notifications" => Op::Notifications,
            "capture" => Op::Capture,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Which {
    Uuid(Uuid),
    /// The Characteristic Value Handle a `discover` reported.
    Handle(u16),
    /// Only `discover`, `notifications` and `capture` allow it.
    None,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleGattArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub op: Op,
    pub which: Which,
    /// The value of a `write`, or the bytes a `notifications` wait looks for.
    pub value: Option<Vec<u8>>,
    pub with_response: bool,
    pub indicate: bool,
    pub within_ms: u64,
    /// `notifications`: virtual time to let pass before reporting, when there is nothing to wait
    /// for.
    pub settle_ms: Option<u64>,
    pub capture_enabled: Option<bool>,
    /// Keeps key material, which taints the instance.
    pub capture_secrets: Option<bool>,
    pub artifact: bool,
    pub label: Option<String>,
    pub timeout: VTime,
    pub wall_budget_ms: u64,
}

/// One artifact-path segment, `^[a-z0-9][a-z0-9._-]*$`.
fn label_is_valid(label: &str) -> bool {
    let mut bytes = label.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && label.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-' || b == b'_'
        })
}

impl BleGattArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<BleGattArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "op",
                "uuid",
                "handle",
                "value",
                "text",
                "contains",
                "settle_ms",
                "with_response",
                "indicate",
                "within_ms",
                "enabled",
                "artifact",
                "label",
                "secrets",
                "timeout_ms",
                "wall_budget_ms",
            ],
        )?;
        let op_text = opt_str(args, "op")?.unwrap_or("discover");
        let op = Op::parse(op_text).ok_or_else(|| {
            usage(
                "op",
                &format!(
                    "`{op_text}` is not one of discover, read, write, subscribe, notifications, \
                     capture"
                ),
            )
        })?;
        let which = match (opt_str(args, "uuid")?, opt_u64(args, "handle")?) {
            (Some(_), Some(_)) => {
                return Err(usage(
                    "handle",
                    "name `uuid` or `handle`, not both: they pick the same characteristic",
                ));
            }
            (Some(text), None) => Which::Uuid(parse_uuid(text, "uuid")?),
            (None, Some(handle)) if handle >= 1 && handle <= u64::from(u16::MAX) => {
                Which::Handle(handle as u16)
            }
            (None, Some(_)) => {
                return Err(usage("handle", "an attribute handle is 1 to 65535"));
            }
            (None, None) => Which::None,
        };
        if matches!(which, Which::None) && matches!(op, Op::Read | Op::Write | Op::Subscribe) {
            return Err(usage(
                "uuid",
                &format!("`{}` names a characteristic by uuid or handle", op.as_str()),
            ));
        }
        // A value is hex (`value`) or a string's UTF-8 bytes (`text`); a wait spells the same pair
        // `contains` and `text`.
        let hex = match op {
            Op::Notifications => opt_str(args, "contains")?,
            _ => opt_str(args, "value")?,
        };
        if op != Op::Notifications && args.contains_key("contains") {
            return Err(usage(
                "contains",
                "only `notifications` waits for bytes; a write names `value`",
            ));
        }
        if op == Op::Notifications && args.contains_key("value") {
            return Err(usage(
                "value",
                "`notifications` looks for bytes with `contains`",
            ));
        }
        let text = opt_str(args, "text")?;
        if hex.is_some() && text.is_some() {
            return Err(usage("text", "name the bytes once, as hex or as `text`"));
        }
        let value = match (hex, text) {
            (Some(hex), _) => Some(parse_hex(hex).ok_or_else(|| {
                usage(
                    if op == Op::Notifications {
                        "contains"
                    } else {
                        "value"
                    },
                    "expected a non-empty even number of hex digits, or `text`",
                )
            })?),
            (None, Some(text)) => Some(text.as_bytes().to_vec()),
            (None, None) => None,
        };
        if op == Op::Notifications && value.is_some() && matches!(which, Which::None) {
            return Err(usage(
                "uuid",
                "a wait names the characteristic whose notifications it watches",
            ));
        }
        if op == Op::Write && value.is_none() {
            return Err(usage("value", "a write names the bytes it writes"));
        }
        if value.is_some() && !matches!(op, Op::Write | Op::Notifications) {
            return Err(usage(
                "value",
                &format!("`{}` writes no bytes", op.as_str()),
            ));
        }
        let label = opt_str(args, "label")?.map(str::to_owned);
        if let Some(label) = label.as_deref()
            && (!label_is_valid(label) || op != Op::Capture)
        {
            return Err(usage(
                "label",
                "a capture label is one path segment of [a-z0-9][a-z0-9._-]*",
            ));
        }
        let within_ms = opt_u64(args, "within_ms")?.unwrap_or(DEFAULT_WITHIN_MS);
        if within_ms == 0 || within_ms > super::ble_scan::TIMEOUT_MS_MAX {
            return Err(usage(
                "within_ms",
                &format!("expected 1 to {} ms", super::ble_scan::TIMEOUT_MS_MAX),
            ));
        }
        let settle_ms = opt_u64(args, "settle_ms")?;
        if let Some(ms) = settle_ms {
            if op != Op::Notifications {
                return Err(usage(
                    "settle_ms",
                    "only `notifications` lets virtual time pass before it reports",
                ));
            }
            if ms == 0 || ms > super::ble_scan::TIMEOUT_MS_MAX {
                return Err(usage(
                    "settle_ms",
                    &format!("expected 1 to {} ms", super::ble_scan::TIMEOUT_MS_MAX),
                ));
            }
            if value.is_some() {
                return Err(usage(
                    "settle_ms",
                    "`contains` already says what to wait for; name one of the two",
                ));
            }
        }
        let capture_enabled = opt_bool(args, "enabled")?;
        if capture_enabled.is_some() && op != Op::Capture {
            return Err(usage("enabled", "only `capture` is turned on and off"));
        }
        let capture_secrets = opt_bool(args, "secrets")?;
        if capture_secrets.is_some() && op != Op::Capture {
            return Err(usage(
                "secrets",
                "only `capture` records with the key material kept",
            ));
        }
        let artifact = opt_bool(args, "artifact")?.unwrap_or(true);
        if args.contains_key("artifact") && op != Op::Capture {
            return Err(usage("artifact", "only `capture` writes an artifact"));
        }
        let timeout = timeout_of(args)?;
        Ok(BleGattArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            op,
            which,
            value,
            with_response: opt_bool(args, "with_response")?.unwrap_or(true),
            indicate: opt_bool(args, "indicate")?.unwrap_or(false),
            within_ms,
            settle_ms,
            capture_enabled,
            capture_secrets,
            artifact,
            label,
            timeout,
            wall_budget_ms: wall_budget_of(args)?,
        })
    }

    /// `ble/<label>.btsnoop`, or `ble/<vt_us>.btsnoop` so two captures of one run never overwrite
    /// each other.
    pub fn artifact_path(&self, vt: VTime) -> String {
        match &self.label {
            Some(label) => format!("{BLE_DIR}/{label}.btsnoop"),
            None => format!("{BLE_DIR}/{}.btsnoop", vt.as_us()),
        }
    }
}

/// Core Vol 3 Part G 3.3.1.1.
pub fn properties_text(properties: u8) -> String {
    const NAMES: [(u8, &str); 8] = [
        (0x01, "broadcast"),
        (0x02, "read"),
        (0x04, "write-without-response"),
        (0x08, "write"),
        (0x10, "notify"),
        (0x20, "indicate"),
        (0x40, "authenticated-signed-writes"),
        (0x80, "extended-properties"),
    ];
    NAMES
        .iter()
        .filter(|(bit, _)| properties & bit != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join(",")
}

/// With its CCCD as a child node when one was discovered.
fn characteristic_json(c: &Characteristic) -> serde_json::Value {
    let mut node = serde_json::json!({
        "kind": "characteristic",
        "uuid": uuid_text(c.uuid),
        "handle": c.value_handle,
        "declaration": c.declaration,
        "properties": properties_text(c.properties),
    });
    if let Some(cccd) = c.cccd {
        node["children"] = serde_json::json!([{
            "kind": "descriptor",
            // Client Characteristic Configuration, Core Vol 3 Part G 3.3.3.3.
            "uuid": uuid_text(Uuid::from_u16(0x2902)),
            "handle": cccd,
        }]);
    }
    node
}

pub fn tree_json(services: &[Service], characteristics: &[Characteristic]) -> serde_json::Value {
    serde_json::Value::Array(
        services
            .iter()
            .map(|s| {
                serde_json::json!({
                    "kind": "service",
                    "uuid": uuid_text(s.uuid),
                    "handle": s.start,
                    "end": s.end,
                    "children": serde_json::Value::Array(
                        characteristics
                            .iter()
                            .filter(|c| c.declaration >= s.start && c.declaration <= s.end)
                            .map(characteristic_json)
                            .collect(),
                    ),
                })
            })
            .collect(),
    )
}

fn notification_json(n: &Notification, characteristics: &[Characteristic]) -> serde_json::Value {
    serde_json::json!({
        "at_us": n.at_us,
        "handle": n.handle,
        "uuid": characteristics
            .iter()
            .find(|c| c.value_handle == n.handle)
            .map(|c| uuid_text(c.uuid)),
        "indication": n.indication,
        "value": hex_text(&n.value),
        "text": String::from_utf8(n.value.clone()).ok(),
    })
}

/// Resolved against what the central discovered.
fn characteristic_of(args: &BleGattArgs, known: &[Characteristic]) -> Result<Uuid, ApiError> {
    match args.which {
        Which::Uuid(uuid) => Ok(uuid),
        Which::Handle(handle) => known
            .iter()
            .find(|c| c.value_handle == handle)
            .map(|c| c.uuid)
            .ok_or_else(|| {
                ApiError::new(
                    E_STATE,
                    format!("the central knows no characteristic with handle {handle:#06x}"),
                )
                .with_hint("`ble_gatt --op discover` refreshes the tree")
            }),
        Which::None => Err(usage("uuid", "this operation names a characteristic")),
    }
}

pub fn ble_gatt_on(session: &mut Session, args: &BleGattArgs) -> Result<Output, ApiError> {
    match args.op {
        Op::Discover => discover(session, args),
        Op::Capture => capture(session, args, crate::artifact_io::installed()),
        _ => one_step(session, args),
    }
}

/// Refused before it is journaled, saying whether the controller runs and whether the advertiser
/// accepts connections at all.
fn connected(session: &mut Session, what: &str) -> Result<(), ApiError> {
    let state = ble_state(session)?;
    if state.central.link.is_some() {
        return Ok(());
    }
    Err(not_running(&state).unwrap_or_else(|| not_connected(what, &state)))
}

fn discover(session: &mut Session, args: &BleGattArgs) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    connected(session, "the service discovery")?;
    let first = run_steps(
        session,
        vec![Step::DiscoverServices],
        args.timeout,
        args.wall_budget_ms,
    )?;
    if !first.all_ok() {
        return Err(step_refused("the service discovery", first.status()));
    }
    let services = first.state.central.services.clone();
    let steps: Vec<Step> = services
        .iter()
        .map(|s| Step::DiscoverCharacteristics { service: s.uuid })
        .collect();
    let ran = if steps.is_empty() {
        first
    } else {
        let mut ran = run_steps(session, steps, args.timeout, args.wall_budget_ms)?;
        ran.elapsed_us = ran.elapsed_us.saturating_add(first.elapsed_us);
        ran
    };
    if !ran.all_ok() {
        return Err(step_refused("the characteristic discovery", ran.status()));
    }
    let services = ran.state.central.services.clone();
    let characteristics = ran.state.central.characteristics.clone();
    let receipt = session.receipt();
    let mut json = common_json(&instance, &ran, &receipt);
    json.insert("op".into(), Op::Discover.as_str().into());
    json.insert("tree".into(), tree_json(&services, &characteristics));
    let mut text = format!(
        "{instance} ble_gatt discover: {} service(s), {} characteristic(s)",
        services.len(),
        characteristics.len()
    );
    for service in &services {
        text.push_str(&format!("\n  service {}", uuid_text(service.uuid)));
        for c in characteristics
            .iter()
            .filter(|c| c.declaration >= service.start && c.declaration <= service.end)
        {
            text.push_str(&format!(
                "\n    char {} @{:#06x} ({}){}",
                uuid_text(c.uuid),
                c.value_handle,
                properties_text(c.properties),
                match c.cccd {
                    Some(cccd) => format!(" cccd @{cccd:#06x}"),
                    None => String::new(),
                }
            ));
        }
    }
    Ok(Output::new(serde_json::Value::Object(json), text, receipt).shaped(&ShapeLimits::DEFAULT))
}

/// One step, or none.
fn one_step(session: &mut Session, args: &BleGattArgs) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    let before = ble_state(session)?;
    let needs_link = match args.op {
        Op::Read | Op::Write | Op::Subscribe => true,
        Op::Notifications => args.value.is_some(),
        Op::Discover | Op::Capture => false,
    };
    if needs_link {
        connected(session, &format!("the {}", args.op.as_str()))?;
    }
    let known = before.central.characteristics.clone();
    let notified_before = before.central.notified;
    let steps = match args.op {
        Op::Read => vec![Step::Read {
            characteristic: characteristic_of(args, &known)?,
        }],
        Op::Write => vec![Step::Write {
            characteristic: characteristic_of(args, &known)?,
            value: args.value.clone().unwrap_or_default(),
            with_response: args.with_response,
        }],
        Op::Subscribe => vec![Step::Subscribe {
            characteristic: characteristic_of(args, &known)?,
            indicate: args.indicate,
        }],
        Op::Notifications => match (&args.value, &args.which) {
            (Some(contains), _) => vec![Step::WaitNotification {
                characteristic: characteristic_of(args, &known)?,
                contains: contains.clone(),
                within_ms: u32::try_from(args.within_ms).unwrap_or(u32::MAX),
            }],
            // Nothing to wait for: `settle_ms` lets the guest run and reports what arrived; without
            // it, a zero-length run reports what is already there, advancing no virtual time.
            (None, _) => match args.settle_ms {
                Some(ms) => vec![Step::Wait {
                    ms: u32::try_from(ms).unwrap_or(u32::MAX),
                }],
                None => Vec::new(),
            },
        },
        Op::Discover | Op::Capture => Vec::new(),
    };
    let ran = if steps.is_empty() {
        let now = session.now();
        session.run_until(now);
        Ran {
            results: Vec::new(),
            state: ble_state(session)?,
            elapsed_us: 0,
        }
    } else {
        run_steps(session, steps, args.timeout, args.wall_budget_ms)?
    };
    if !ran.all_ok() {
        return Err(step_refused(
            &format!("the {}", args.op.as_str()),
            ran.status(),
        ));
    }
    let characteristics = ran.state.central.characteristics.clone();
    let named = match args.which {
        Which::None => None,
        _ => characteristic_of(args, &characteristics)
            .or_else(|_| characteristic_of(args, &known))
            .ok(),
    };
    let receipt = session.receipt();
    let mut json = common_json(&instance, &ran, &receipt);
    json.insert("op".into(), args.op.as_str().into());
    json.insert(
        "uuid".into(),
        named.map_or(serde_json::Value::Null, |u| uuid_text(u).into()),
    );
    let mut text = format!("{instance} ble_gatt {}", args.op.as_str());
    match args.op {
        Op::Read => {
            let value = ran
                .results
                .first()
                .map(|r| r.value.clone())
                .unwrap_or_default();
            json.insert("value".into(), hex_text(&value).into());
            json.insert(
                "text".into(),
                String::from_utf8(value.clone())
                    .ok()
                    .map_or(serde_json::Value::Null, serde_json::Value::from),
            );
            text.push_str(&format!(
                " {}: {} byte(s) {}",
                named.map_or_else(String::new, uuid_text),
                value.len(),
                hex_text(&value)
            ));
        }
        Op::Write => {
            let written = args.value.as_ref().map_or(0, Vec::len);
            json.insert("bytes".into(), written.into());
            json.insert("with_response".into(), args.with_response.into());
            text.push_str(&format!(
                " {}: {written} byte(s) {}",
                named.map_or_else(String::new, uuid_text),
                if args.with_response {
                    "with response"
                } else {
                    "without response"
                }
            ));
        }
        Op::Subscribe => {
            let cccd = named.and_then(|u| {
                characteristics
                    .iter()
                    .find(|c| c.uuid == u)
                    .and_then(|c| c.cccd)
            });
            json.insert(
                "cccd".into(),
                cccd.map_or(serde_json::Value::Null, serde_json::Value::from),
            );
            json.insert("indicate".into(), args.indicate.into());
            text.push_str(&format!(
                " {}: {} through CCCD {}",
                named.map_or_else(String::new, uuid_text),
                if args.indicate {
                    "indications"
                } else {
                    "notifications"
                },
                cccd.map_or_else(|| "(none)".to_owned(), |h| format!("{h:#06x}"))
            ));
        }
        _ => {}
    }
    if args.op == Op::Notifications {
        let all = &ran.state.central.notifications;
        let fresh = usize::try_from(ran.state.central.notified.saturating_sub(notified_before))
            .unwrap_or(all.len())
            .min(all.len());
        let listed: Vec<serde_json::Value> = all
            .iter()
            .filter(|n| {
                named.is_none_or(|u| {
                    characteristics
                        .iter()
                        .any(|c| c.uuid == u && c.value_handle == n.handle)
                })
            })
            .map(|n| notification_json(n, &characteristics))
            .collect();
        json.insert("notifications".into(), serde_json::Value::Array(listed));
        json.insert("new".into(), fresh.into());
        json.insert("total".into(), ran.state.central.notified.into());
        text.push_str(&format!(
            ": {} new, {} kept",
            fresh,
            ran.state.central.notifications.len()
        ));
        for n in all.iter().rev().take(fresh) {
            text.push_str(&format!(
                "\n  @{:#06x} {}",
                n.handle,
                String::from_utf8(n.value.clone()).unwrap_or_else(|_| hex_text(&n.value))
            ));
        }
    }
    Ok(Output::new(serde_json::Value::Object(json), text, receipt).shaped(&ShapeLimits::DEFAULT))
}

pub fn capture(
    session: &mut Session,
    args: &BleGattArgs,
    io: Option<crate::artifact_io::ArtifactIo>,
) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    // The mode goes first: `set_secrets` drops what the capture holds, so turning recording and
    // full fidelity on in one call records nothing the new mode did not.
    let mut steps: Vec<Step> = Vec::new();
    if let Some(secrets) = args.capture_secrets {
        steps.push(Step::CaptureSecrets { secrets });
    }
    if let Some(enabled) = args.capture_enabled {
        steps.push(Step::Capture { enabled });
    }
    let ran = if steps.is_empty() {
        let now = session.now();
        session.run_until(now);
        Ran {
            results: Vec::new(),
            state: ble_state(session)?,
            elapsed_us: 0,
        }
    } else {
        // A capture step never reaches the central, so `run_steps` waits for no result; its
        // zero-length run applies the input.
        let ran = run_steps(session, steps, args.timeout, args.wall_budget_ms)?;
        let now = session.now();
        session.run_until(now);
        Ran {
            results: ran.results,
            state: ble_state(session)?,
            elapsed_us: ran.elapsed_us,
        }
    };
    let capture = &ran.state.capture;
    let mut artifact = None;
    if args.artifact {
        let io = io.ok_or_else(no_writer)?;
        // The value pass runs before the bytes are written and hashed. Key material is already
        // gone, so what it catches is a MAC or credential the guest put on the wire. `secrets_of`
        // also reads the full-fidelity mode, so a call that turns it on is itself tainted.
        let set = super::snapshot::secrets_of(session);
        let bytes = crate::redact::Redactor::new(&set).redact_bytes(&capture.to_btsnoop());
        let path = args.artifact_path(session.now());
        let written = (io.write)(&path, &bytes).map_err(|err| {
            ApiError::new(E_STATE, format!("the capture could not be written: {err}"))
        })?;
        let sha256 = super::snapshot::sha256_hex(&bytes);
        artifact = Some(
            ArtifactRef::new(written, sha256, BTSNOOP_MEDIA_TYPE, bytes.len() as u64).map_err(
                |err| {
                    ApiError::new(
                        E_INTERNAL,
                        format!("the artifact path is not usable: {err}"),
                    )
                },
            )?,
        );
    }
    // A tainted instance's capture says so in its receipt. A call that asked for the mode but wrote
    // no artifact has not run `secrets_of` above, so the set (and the taint) is brought up to date
    // here.
    let _ = super::snapshot::secrets_of(session);
    let receipt = session.receipt();
    let mut json = common_json(&instance, &ran, &receipt);
    json.insert("op".into(), Op::Capture.as_str().into());
    json.insert("enabled".into(), capture.enabled.into());
    json.insert("packets".into(), capture.records.len().into());
    json.insert("bytes".into(), capture.bytes.into());
    json.insert("dropped".into(), capture.dropped.into());
    json.insert("key_material_zeroed".into(), capture.redacted.into());
    json.insert("secrets".into(), capture.secrets.into());
    json.insert("datalink".into(), btsnoop::DATALINK_H4.into());
    json.insert(
        "capture".into(),
        artifact
            .as_ref()
            .map_or(serde_json::Value::Null, ArtifactRef::to_json),
    );
    let mut text = if capture.secrets {
        format!(
            "{instance} ble_gatt capture {} with key material kept (tainted): {} packet(s), {} \
             byte(s), {} dropped",
            if capture.enabled { "on" } else { "off" },
            capture.records.len(),
            capture.bytes,
            capture.dropped
        )
    } else {
        format!(
            "{instance} ble_gatt capture {}: {} packet(s), {} byte(s), {} dropped, {} with key \
             material zeroed",
            if capture.enabled { "on" } else { "off" },
            capture.records.len(),
            capture.bytes,
            capture.dropped,
            capture.redacted
        )
    };
    if let Some(artifact) = &artifact {
        text.push_str(&format!(" -> {}", artifact.path));
    }
    let mut output = Output::new(serde_json::Value::Object(json), text, receipt);
    if let Some(artifact) = artifact {
        output = output.with_artifact(artifact).map_err(|err| {
            ApiError::new(
                E_INTERNAL,
                format!("the artifact path is not usable: {err}"),
            )
        })?;
    }
    Ok(output.shaped(&ShapeLimits::DEFAULT))
}

/// Names who installs one.
fn no_writer() -> ApiError {
    ApiError::new(
        E_STATE,
        "this build cannot write an artifact, so the capture has nowhere to go",
    )
    .with_hint(
        "pass `artifact: false` to read the capture's counters only; a host installs the writer \
         with `artifact_io::set`",
    )
}

pub fn input_schema() -> Schema {
    let mut properties = common_properties();
    for (key, value) in [
        (
            "op",
            serde_json::json!({
                "type": "string",
                "enum": ["discover", "read", "write", "subscribe", "notifications", "capture"],
                "description": "The GATT operation; the default is discover."
            }),
        ),
        (
            "uuid",
            serde_json::json!({"type": "string", "description": "Characteristic UUID."}),
        ),
        (
            "handle",
            serde_json::json!({
                "type": "integer", "minimum": 1, "maximum": 65535,
                "description": "Characteristic Value Handle, as a discover reported it."
            }),
        ),
        (
            "value",
            serde_json::json!({"type": "string", "description": "Hex bytes to write."}),
        ),
        (
            "text",
            serde_json::json!({
                "type": "string",
                "description": "The bytes as UTF-8 text instead of hex."
            }),
        ),
        (
            "contains",
            serde_json::json!({
                "type": "string",
                "description": "Hex bytes a `notifications` call waits for."
            }),
        ),
        (
            "with_response",
            serde_json::json!({
                "type": "boolean", "default": true,
                "description": "ATT_WRITE_REQ rather than ATT_WRITE_CMD; `--no-with-response` sends ATT_WRITE_CMD."
            }),
        ),
        (
            "indicate",
            serde_json::json!({
                "type": "boolean",
                "description": "Subscribe to indications rather than notifications."
            }),
        ),
        (
            "within_ms",
            serde_json::json!({
                "type": "integer", "minimum": 1,
                "description": "How long a `notifications` wait runs."
            }),
        ),
        (
            "settle_ms",
            serde_json::json!({
                "type": "integer", "minimum": 1,
                "description": "`notifications`: let this much virtual time pass, then report what arrived."
            }),
        ),
        (
            "enabled",
            serde_json::json!({
                "type": "boolean",
                "description": "`capture`: record packets from now on, or stop and drop what is held."
            }),
        ),
        (
            "secrets",
            serde_json::json!({
                "type": "boolean",
                "description": "`capture`: record with the key material kept, for a pairing trace. Taints the instance, so `snapshot export` then needs the confirmation, and drops what the capture already holds."
            }),
        ),
        (
            "artifact",
            serde_json::json!({
                "type": "boolean", "default": true,
                "description": "`capture`: write the btsnoop file; `--no-artifact` reports the counters only."
            }),
        ),
        (
            "label",
            serde_json::json!({
                "type": "string",
                "description": "`capture`: the artifact's file stem under ble/."
            }),
        ),
    ] {
        properties.insert(key.into(), value);
    }
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`ble_gatt` arguments.",
        "properties": properties
    })
}

pub fn output_schema() -> Schema {
    schemars::json_schema!({
        "type": "object",
        "properties": {
            "instance": { "type": "string" },
            "vt_us": { "type": "integer" },
            "elapsed_vt_us": { "type": "integer" },
            "status": { "type": "string" },
            "op": { "type": "string" },
            "steps": { "type": "array", "items": { "type": "object" } },
            "radio": super::ble_scan::radio_schema(),
            "tree": { "type": "array", "items": { "type": "object" } },
            "uuid": { "type": ["string", "null"] },
            "value": { "type": "string" },
            "text": { "type": ["string", "null"] },
            "bytes": { "type": "integer" },
            "cccd": { "type": ["integer", "null"] },
            "notifications": { "type": "array", "items": { "type": "object" } },
            "new": { "type": "integer" },
            "total": { "type": "integer" },
            "enabled": { "type": "boolean" },
            "packets": { "type": "integer" },
            "dropped": { "type": "integer" },
            "key_material_zeroed": { "type": "integer" },
            "secrets": { "type": "boolean" },
            "datalink": { "type": "integer" },
            "capture": {
                "type": ["object", "null"],
                "properties": {
                    "path": { "type": "string" },
                    "sha256": { "type": "string" },
                    "media_type": { "type": "string" },
                    "bytes": { "type": "integer" }
                }
            }
        }
    })
}

/// Run the scripted BLE central's GATT client, or take its btsnoop capture.
#[command(
    api_crate = crate,
    name = "ble_gatt",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["op"]),
    scenario_step = "ble.gatt",
    errors(E_USAGE, E_STATE, E_LEASE, E_TIMEOUT, E_WALL_BUDGET, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Discover the services and characteristics of the connected peer",
        args = r#"{"op":"discover"}"#,
    ),
    example(
        title = "Subscribe to the Passport Keys events characteristic",
        args = r#"{"op":"subscribe","uuid":"12D4FA09-7418-48FA-A95A-B43A2E669E55"}"#,
    ),
    example(
        title = "Write a command line and wait for its answer",
        args = r#"{"op":"write","uuid":"12D4FA0A-7418-48FA-A95A-B43A2E669E55","text":"{\"cmd\":\"ping\"}\n"}"#,
    ),
    example(
        title = "Write the btsnoop capture as an artifact",
        args = r#"{"op":"capture","label":"gatt"}"#,
    ),
    example(
        title = "Record the key material too, for a pairing trace, which taints the instance",
        args = r#"{"op":"capture","secrets":true,"artifact":false}"#,
    ),
)]
pub fn ble_gatt(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = BleGattArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_BLE_GATT.annotations, args.instance.as_deref()),
        |session| ble_gatt_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(value: serde_json::Value) -> BleGattArgs {
        BleGattArgs::from_json(&value).expect("the arguments parse")
    }

    #[test]
    fn the_default_operation_is_discover_and_names_no_characteristic() {
        let parsed = args(serde_json::json!({}));
        assert_eq!(parsed.op, Op::Discover);
        assert_eq!(parsed.which, Which::None);
        assert!(parsed.artifact);
        assert!(parsed.with_response);
    }

    #[test]
    fn a_value_is_hex_or_text_and_never_both() {
        let uuid = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";
        assert_eq!(
            args(serde_json::json!({"op":"write","uuid":uuid,"value":"6869"})).value,
            Some(vec![0x68, 0x69])
        );
        assert_eq!(
            args(serde_json::json!({"op":"write","uuid":uuid,"text":"hi\n"})).value,
            Some(b"hi\n".to_vec())
        );
        for bad in [
            serde_json::json!({"op":"write","uuid":uuid}),
            serde_json::json!({"op":"write","uuid":uuid,"value":"6869","text":"hi"}),
            serde_json::json!({"op":"write","uuid":uuid,"value":"abc"}),
            serde_json::json!({"op":"write"}),
            serde_json::json!({"op":"read","uuid":uuid,"value":"6869"}),
            serde_json::json!({"op":"read","uuid":uuid,"handle":12}),
            serde_json::json!({"op":"nonsense"}),
            serde_json::json!({"op":"discover","contains":"6869"}),
            serde_json::json!({"op":"notifications","value":"6869"}),
            serde_json::json!({"op":"discover","enabled":true}),
            serde_json::json!({"op":"discover","artifact":false}),
            serde_json::json!({"op":"notifications","contains":"6869"}),
            serde_json::json!({"op":"notifications","settle_ms":250,"contains":"6869"}),
            serde_json::json!({"op":"read","uuid":"2a00","settle_ms":250}),
            serde_json::json!({"op":"capture","label":"Not A Segment"}),
        ] {
            assert_eq!(
                BleGattArgs::from_json(&bad).expect_err("refused").code,
                E_USAGE,
                "{bad}"
            );
        }
    }

    /// The opt-in is read only for `capture`, so no other operation can quietly accept it.
    #[test]
    fn a_full_fidelity_capture_is_asked_for_by_name_and_only_of_a_capture() {
        let args = BleGattArgs::from_json(&serde_json::json!({"op":"capture","secrets":true}))
            .expect("a capture takes `secrets`");
        assert_eq!(args.capture_secrets, Some(true));
        let off = BleGattArgs::from_json(&serde_json::json!({"op":"capture","secrets":false}))
            .expect("a capture takes `secrets`");
        assert_eq!(off.capture_secrets, Some(false));
        let plain = BleGattArgs::from_json(&serde_json::json!({"op":"capture"}))
            .expect("a capture without it");
        assert_eq!(plain.capture_secrets, None);
        for op in ["discover", "read", "subscribe", "notifications"] {
            let error = BleGattArgs::from_json(
                &serde_json::json!({"op":op,"uuid":super::super::ble_scan::PK_SERVICE,"secrets":true}),
            )
            .expect_err("only `capture` records");
            assert_eq!(error.code, E_USAGE);
            assert!(error.message.contains("key material"), "{}", error.message);
        }
    }

    #[test]
    fn the_tree_nests_characteristics_and_cccds_under_their_service() {
        let service = Service {
            start: 0x0010,
            end: 0x0020,
            uuid: parse_uuid(super::super::ble_scan::PK_SERVICE, "s").expect("a UUID"),
        };
        let characteristic = Characteristic {
            declaration: 0x0011,
            properties: 0x10 | 0x08,
            value_handle: 0x0012,
            uuid: parse_uuid("12D4FA09-7418-48FA-A95A-B43A2E669E55", "c").expect("a UUID"),
            cccd: Some(0x0013),
        };
        let tree = tree_json(&[service], &[characteristic]);
        assert_eq!(tree[0]["kind"], "service");
        assert_eq!(tree[0]["children"][0]["kind"], "characteristic");
        assert_eq!(tree[0]["children"][0]["handle"], 0x12);
        assert_eq!(tree[0]["children"][0]["properties"], "write,notify");
        assert_eq!(tree[0]["children"][0]["children"][0]["handle"], 0x13);
        assert_eq!(
            tree[0]["children"][0]["children"][0]["uuid"],
            "00002902-0000-1000-8000-00805F9B34FB"
        );
    }

    #[test]
    fn the_artifact_path_is_one_segment_under_ble() {
        let named = args(serde_json::json!({"op":"capture","label":"gatt"}));
        assert_eq!(named.artifact_path(VTime(0)), "ble/gatt.btsnoop");
        let anonymous = args(serde_json::json!({"op":"capture"}));
        assert_eq!(
            anonymous.artifact_path(VTime::from_ms(5)),
            "ble/5000.btsnoop"
        );
    }

    #[test]
    fn the_command_is_in_the_radio_group_and_its_examples_parse() {
        let spec = crate::registry::find("ble_gatt").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let value: serde_json::Value =
                serde_json::from_str(example.args).expect("the example is JSON");
            BleGattArgs::from_json(&value).expect("every example parses");
        }
    }
}
