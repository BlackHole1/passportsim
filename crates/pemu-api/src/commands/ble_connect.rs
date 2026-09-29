//! `passportsim ble_connect`: the scripted central opens or ends its one connection. The half
//! shared with `ble_scan` and `ble_gatt` is in `ble_scan.rs`.
//!
//! The target is `addr` (that advertiser), `service` (the first connectable advertiser listing that
//! UUID) or neither (the first connectable advertiser). `random` (the advertiser's `TxAdd`) comes
//! from a scan report that heard the address, so an address pasted from `ble_scan` needs no type;
//! otherwise it defaults to public, which the emulated peripheral has.
//!
//! One call journals `Connect` then `ExchangeMtu`: the default ATT MTU of 23 (Core Vol 3 Part F
//! 3.2.8) is too small for the GATT commands' values. `mtu: 23` asks for the default exchange.
//!
//! `disconnect: true` journals `Step::Disconnect` (`LL_TERMINATE_IND`, Core Vol 6 Part B 5.1.6).
//! There is one connection at a time, so the `addr` the web card sends with it is only checked.

use pemu_core::time::VTime;
use pemu_radio::ble::central::{AdvReport, Step, Target};

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TIMEOUT,
    E_TRIPWIRE, E_USAGE, E_WALL_BUDGET,
};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use pemu_radio::ble::central::StepStatus;
use pemu_radio::ble::vhci::BleState;

use super::ble_scan::{
    self, PK_SERVICE, Ran, addr_text, ble_state, common_json, common_properties, connectable,
    controller_up, non_connectable, on_air, parse_addr, parse_uuid, pdu_name, report_json,
    report_name, run_steps, status_name, step_refused, timeout_of, uuid_text, wall_budget_of, why,
};
use super::nfc_tap::bind_checked;
use crate::args::{object, only, opt_bool, opt_str, opt_u64, usage};
use crate::session::Session;

/// In 1.25 ms units: 30 ms. Class C, this central's own choice.
pub const DEFAULT_INTERVAL: u16 = 24;
pub const DEFAULT_LATENCY: u16 = 0;
/// In 10 ms units: 4 s. Class C.
pub const DEFAULT_SUPERVISION_TIMEOUT: u16 = 400;
pub const DEFAULT_WITHIN_MS: u64 = 2_000;
/// A common central's receive MTU.
pub const DEFAULT_MTU: u16 = 247;
/// The largest attribute value plus the 3-octet write header (Core Vol 3 Part F 3.2.9).
pub const MTU_MAX: u16 = 517;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Peer {
    Any,
    /// A display-form address, and the `random` flag a call gave, if any.
    Addr(String, Option<bool>),
    Service(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleConnectArgs {
    /// `None` while exactly one is live.
    pub instance: Option<String>,
    pub disconnect: bool,
    pub peer: Peer,
    /// In 1.25 ms units.
    pub interval: u16,
    /// In connection events.
    pub latency: u16,
    /// In 10 ms units.
    pub supervision_timeout: u16,
    /// In milliseconds.
    pub within_ms: u64,
    /// `None` leaves the link at the default ATT MTU without an exchange.
    pub mtu: Option<u16>,
    pub timeout: VTime,
    pub wall_budget_ms: u64,
}

/// Refused outside `min..=max`.
fn opt_u16(
    args: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    (min, max): (u16, u16),
    default: u16,
) -> Result<u16, ApiError> {
    match opt_u64(args, key)? {
        None => Ok(default),
        Some(value) if value >= u64::from(min) && value <= u64::from(max) => Ok(value as u16),
        Some(_) => Err(usage(
            key,
            &format!("expected a value between {min} and {max}"),
        )),
    }
}

impl BleConnectArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<BleConnectArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "addr",
                "random",
                "service",
                "disconnect",
                "interval",
                "latency",
                "supervision_timeout",
                "within_ms",
                "mtu",
                "timeout_ms",
                "wall_budget_ms",
            ],
        )?;
        let addr = opt_str(args, "addr")?.map(str::to_owned);
        let service = opt_str(args, "service")?.map(str::to_owned);
        let disconnect = opt_bool(args, "disconnect")?.unwrap_or(false);
        if let Some(addr) = addr.as_deref()
            && parse_addr(addr).is_none()
        {
            return Err(usage(
                "addr",
                &format!("`{addr}` is not a BLE address; write 02:00:00:00:00:01"),
            ));
        }
        if let Some(service) = service.as_deref() {
            resolve_service(service)?;
        }
        if addr.is_some() && service.is_some() {
            return Err(usage(
                "service",
                "name `addr` or `service`, not both: they are two ways to pick one advertiser",
            ));
        }
        let peer = match (addr, service) {
            (Some(addr), _) => Peer::Addr(addr, opt_bool(args, "random")?),
            (None, Some(service)) => Peer::Service(service),
            (None, None) => Peer::Any,
        };
        if disconnect && matches!(peer, Peer::Service(_)) {
            return Err(usage(
                "service",
                "a disconnect names no advertiser: there is one connection at a time",
            ));
        }
        let within_ms = opt_u64(args, "within_ms")?.unwrap_or(DEFAULT_WITHIN_MS);
        if within_ms == 0 || within_ms > ble_scan::TIMEOUT_MS_MAX {
            return Err(usage(
                "within_ms",
                &format!("expected 1 to {} ms", ble_scan::TIMEOUT_MS_MAX),
            ));
        }
        let mtu = match opt_u64(args, "mtu")? {
            None => Some(DEFAULT_MTU),
            // 0 is "leave the link at the default ATT MTU and exchange nothing".
            Some(0) => None,
            Some(v) if v >= 23 && v <= u64::from(MTU_MAX) => Some(v as u16),
            Some(_) => {
                return Err(usage(
                    "mtu",
                    &format!("an ATT MTU is 0 (no exchange) or 23 to {MTU_MAX}"),
                ));
            }
        };
        let timeout = timeout_of(args)?;
        if !disconnect && VTime::from_ms(within_ms).0 > timeout.0 {
            return Err(usage(
                "within_ms",
                "the connect wait is longer than the call's timeout_ms",
            ));
        }
        Ok(BleConnectArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            disconnect,
            peer,
            // Core Vol 4 Part E 7.8.12 bounds: interval 6 to 3,200 (7.5 ms to 4 s), latency 0 to
            // 499, supervision timeout 10 to 3,200 (100 ms to 32 s).
            interval: opt_u16(args, "interval", (6, 3_200), DEFAULT_INTERVAL)?,
            latency: opt_u16(args, "latency", (0, 499), DEFAULT_LATENCY)?,
            supervision_timeout: opt_u16(
                args,
                "supervision_timeout",
                (10, 3_200),
                DEFAULT_SUPERVISION_TIMEOUT,
            )?,
            within_ms,
            mtu,
            timeout,
            wall_budget_ms: wall_budget_of(args)?,
        })
    }
}

/// `pk` is the Passport Keys vendor service; anything else is a UUID.
fn resolve_service(text: &str) -> Result<pemu_radio::ble::central::Uuid, ApiError> {
    if text.eq_ignore_ascii_case("pk") {
        return parse_uuid(PK_SERVICE, "service");
    }
    parse_uuid(text, "service")
}

/// Also the advertiser report it was resolved against, if the central heard one.
fn target_of(
    args: &BleConnectArgs,
    reports: &[AdvReport],
) -> Result<(Target, Option<AdvReport>), ApiError> {
    Ok(match &args.peer {
        Peer::Any => {
            let heard = reports.iter().find(|r| connectable(r.pdu_type)).cloned();
            (Target::Any, heard)
        }
        Peer::Addr(text, random) => {
            let address = parse_addr(text).ok_or_else(|| usage("addr", "not a BLE address"))?;
            let heard = reports.iter().rev().find(|r| r.address == address).cloned();
            let random = random
                .or_else(|| heard.as_ref().map(|r| r.random))
                .unwrap_or(false);
            (Target::Address { address, random }, heard)
        }
        Peer::Service(text) => {
            let uuid = resolve_service(text)?;
            let heard = reports
                .iter()
                .find(|r| {
                    connectable(r.pdu_type)
                        && (pemu_radio::ble::central::ad_lists_service(&r.data, uuid)
                            || r.scan_response.as_deref().is_some_and(|d| {
                                pemu_radio::ble::central::ad_lists_service(d, uuid)
                            }))
                })
                .cloned();
            (Target::Service(uuid), heard)
        }
    })
}

/// A connection to the firmware's advertiser while it advertises a non-connectable PDU is refused
/// before anything is journaled: the advertising type is the guest's choice, so waiting would only
/// spend the timeout. A wait that runs out says what went wrong (nothing advertised, the named peer
/// not heard, a connectable peer did not answer).
pub fn ble_connect_on(session: &mut Session, args: &BleConnectArgs) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    if args.disconnect {
        let before = ble_state(session)?;
        if before.central.link.is_none() {
            return Err(
                ApiError::new(E_STATE, "the virtual central has no connection to end")
                    .with_hint("`ble_connect` with an address opens one"),
            );
        }
        let ran = run_steps(
            session,
            vec![Step::Disconnect],
            args.timeout,
            args.wall_budget_ms,
        )?;
        if !ran.all_ok() {
            return Err(step_refused("the disconnect", ran.status()));
        }
        return Ok(output(session, &instance, &ran, args, None));
    }
    let before = controller_up(session)?;
    let (target, heard) = target_of(args, &before.central.reports)?;
    if let Some(refusal) = refuse_at_once(&args.peer, &before) {
        return Err(refusal);
    }
    let mut steps = vec![Step::Connect {
        target,
        interval: args.interval,
        latency: args.latency,
        timeout: args.supervision_timeout,
        within_ms: u32::try_from(args.within_ms).unwrap_or(u32::MAX),
    }];
    if let Some(mtu) = args.mtu {
        steps.push(Step::ExchangeMtu { mtu });
    }
    let started_us = session.now().as_us();
    let ran = run_steps(session, steps, args.timeout, args.wall_budget_ms)?;
    if !ran.all_ok() {
        let connect = ran.results.first().map(|r| r.status);
        return Err(if connect == Some(StepStatus::Timeout) {
            wait_ran_out(args, &ran.state, started_us)
        } else {
            step_refused("the connection", ran.status())
        });
    }
    // As the central last heard it.
    let peer = heard.or_else(|| {
        ran.state
            .central
            .reports
            .iter()
            .rev()
            .find(|r| r.at_us >= started_us && connectable(r.pdu_type))
            .cloned()
    });
    Ok(output(session, &instance, &ran, args, peer.as_ref()))
}

/// `None` when the wait is worth running. `Any` and `Service` can only mean the firmware's
/// advertiser (the one advertiser on the air), and an address means it when the address is its own.
fn refuse_at_once(peer: &Peer, state: &BleState) -> Option<ApiError> {
    let adv = on_air(state)?;
    if connectable(adv.pdu_type) {
        return None;
    }
    let addr = addr_text(adv.adv_a);
    let named = match peer {
        Peer::Any | Peer::Service(_) => true,
        Peer::Addr(text, _) => parse_addr(text) == Some(adv.adv_a),
    };
    named.then(|| non_connectable(&addr, adv.pdu_type))
}

/// Names what the air carried during the wait: a report at or after `started_us` was heard during
/// it.
fn wait_ran_out(args: &BleConnectArgs, state: &BleState, started_us: u64) -> ApiError {
    let heard: Vec<&AdvReport> = state
        .central
        .reports
        .iter()
        .filter(|r| r.at_us >= started_us)
        .collect();
    let wait = args.within_ms;
    let named = match &args.peer {
        Peer::Addr(text, _) => parse_addr(text),
        Peer::Any | Peer::Service(_) => None,
    };
    let candidates: Vec<&AdvReport> = heard
        .iter()
        .copied()
        .filter(|r| named.is_none_or(|a| r.address == a))
        .collect();
    let list = heard
        .iter()
        .map(|r| format!("{} ({})", addr_text(r.address), pdu_name(r.pdu_type)))
        .collect::<Vec<_>>()
        .join(", ");
    if heard.is_empty() {
        return ApiError::new(
            E_STATE,
            format!(
                "the connection did not complete: nothing advertised during the {wait} ms wait,                  so there was no one to connect to"
            ),
        )
        .with_hint("the firmware starts advertising on its own; `ble_scan` shows the air")
        .with_detail(serde_json::json!({ "ble": why::NOT_ADVERTISING }));
    }
    if candidates.is_empty() {
        let who = match &args.peer {
            Peer::Addr(text, _) => format!("no advertiser with address {text}"),
            Peer::Any | Peer::Service(_) => "no matching advertiser".to_owned(),
        };
        return ApiError::new(
            E_STATE,
            format!(
                "the connection did not complete: {who} was heard during the {wait} ms wait; the                  air carried {list}"
            ),
        )
        .with_hint("`ble_scan` lists the addresses on the air")
        .with_detail(serde_json::json!({ "ble": why::PEER_NOT_SEEN }));
    }
    if let Some(r) = candidates.iter().find(|r| !connectable(r.pdu_type))
        && !candidates.iter().any(|r| connectable(r.pdu_type))
    {
        return non_connectable(&addr_text(r.address), r.pdu_type);
    }
    let peer = candidates
        .iter()
        .find(|r| connectable(r.pdu_type))
        .map_or_else(String::new, |r| format!("{} ", addr_text(r.address)));
    ApiError::new(
        E_STATE,
        format!(
            "the connection did not complete: {peer}advertised connectably and the connection did              not come up within the {wait} ms wait"
        ),
    )
    .retryable()
    .with_hint("a longer `within_ms` waits more advertising events")
    .with_detail(serde_json::json!({ "ble": why::NO_ANSWER }))
}

fn output(
    session: &mut Session,
    instance: &str,
    ran: &Ran,
    args: &BleConnectArgs,
    peer: Option<&AdvReport>,
) -> Output {
    let link = ran.state.central.link.clone();
    let receipt = session.receipt();
    let mut json = common_json(instance, ran, &receipt);
    json.insert("connected".into(), link.is_some().into());
    json.insert(
        "handle".into(),
        link.as_ref()
            .map_or(serde_json::Value::Null, |l| l.handle.into()),
    );
    json.insert(
        "mtu".into(),
        link.as_ref()
            .map_or(serde_json::Value::Null, |l| l.mtu.into()),
    );
    json.insert(
        "peer".into(),
        peer.map_or(serde_json::Value::Null, report_json),
    );
    json.insert(
        "target".into(),
        match &args.peer {
            Peer::Any => "any".into(),
            Peer::Addr(addr, _) => serde_json::Value::from(addr.as_str()),
            Peer::Service(service) => serde_json::Value::from(
                resolve_service(service).map_or_else(|_| service.clone(), uuid_text),
            ),
        },
    );
    let text = match (&link, args.disconnect) {
        (_, true) => format!("{instance} ble_connect: the link is down"),
        (Some(l), false) => format!(
            "{instance} ble_connect {} ({}): handle {:#06x}, ATT MTU {}",
            peer.map_or_else(
                || match &args.peer {
                    Peer::Addr(addr, _) => addr.clone(),
                    Peer::Service(_) | Peer::Any => "the advertiser".to_owned(),
                },
                |r| addr_text(r.address)
            ),
            peer.and_then(report_name)
                .unwrap_or_else(|| "no name".to_owned()),
            l.handle,
            l.mtu
        ),
        (None, false) => format!(
            "{instance} ble_connect: no connection ({})",
            status_name(ran.status())
        ),
    };
    Output::new(serde_json::Value::Object(json), text, receipt).shaped(&ShapeLimits::DEFAULT)
}

pub fn input_schema() -> Schema {
    let mut properties = common_properties();
    for (key, value) in [
        (
            "addr",
            serde_json::json!({
                "type": "string",
                "description": "Advertiser address, most significant octet first (02:00:00:00:00:01)."
            }),
        ),
        (
            "random",
            serde_json::json!({
                "type": "boolean",
                "description": "The address is random (TxAdd). Taken from the scan report when one is known."
            }),
        ),
        (
            "service",
            serde_json::json!({
                "type": "string",
                "description": "Connect to the first connectable advertiser listing this service UUID; `pk` is the Passport Keys vendor service."
            }),
        ),
        (
            "disconnect",
            serde_json::json!({
                "type": "boolean",
                "description": "End the connection instead of opening one."
            }),
        ),
        (
            "interval",
            serde_json::json!({
                "type": "integer", "minimum": 6, "maximum": 3200,
                "description": "Connection_Interval in 1.25 ms units (Core Vol 4 Part E 7.8.12)."
            }),
        ),
        (
            "latency",
            serde_json::json!({
                "type": "integer", "minimum": 0, "maximum": 499,
                "description": "Max_Latency in connection events."
            }),
        ),
        (
            "supervision_timeout",
            serde_json::json!({
                "type": "integer", "minimum": 10, "maximum": 3200,
                "description": "Supervision_Timeout in 10 ms units."
            }),
        ),
        (
            "within_ms",
            serde_json::json!({
                "type": "integer", "minimum": 1,
                "description": "How long to wait for a connectable advertisement."
            }),
        ),
        (
            "mtu",
            serde_json::json!({
                "type": "integer", "minimum": 0, "maximum": MTU_MAX,
                "description": "Client receive MTU to exchange once connected; 0 exchanges none."
            }),
        ),
    ] {
        properties.insert(key.into(), value);
    }
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`ble_connect` arguments.",
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
            "steps": { "type": "array", "items": { "type": "object" } },
            "radio": super::ble_scan::radio_schema(),
            "connected": { "type": "boolean" },
            "handle": { "type": ["integer", "null"] },
            "mtu": { "type": ["integer", "null"] },
            "target": { "type": "string" },
            "peer": { "type": ["object", "null"] }
        }
    })
}

/// Connect the scripted BLE central to an advertiser, or end its connection.
#[command(
    api_crate = crate,
    name = "ble_connect",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["addr"]),
    scenario_step = "ble.connect",
    errors(E_USAGE, E_STATE, E_LEASE, E_TIMEOUT, E_WALL_BUDGET, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Connect to the Passport Keys vendor service",
        args = r#"{"service":"pk"}"#,
    ),
    example(
        title = "Connect to an address a scan reported",
        args = r#"{"addr":"02:00:00:00:00:01"}"#,
    ),
    example(
        title = "End the connection",
        args = r#"{"disconnect":true}"#,
    ),
)]
pub fn ble_connect(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = BleConnectArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_BLE_CONNECT.annotations, args.instance.as_deref()),
        |session| ble_connect_on(session, &args),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use pemu_radio::ble::air;

    fn args(value: serde_json::Value) -> BleConnectArgs {
        BleConnectArgs::from_json(&value).expect("the arguments parse")
    }

    #[test]
    fn the_defaults_are_the_parameters_the_gatt_exchange_uses() {
        let parsed = args(serde_json::json!({}));
        assert_eq!(parsed.interval, 24);
        assert_eq!(parsed.latency, 0);
        assert_eq!(parsed.supervision_timeout, 400);
        assert_eq!(parsed.within_ms, DEFAULT_WITHIN_MS);
        assert_eq!(parsed.mtu, Some(DEFAULT_MTU));
        assert_eq!(parsed.peer, Peer::Any);
    }

    #[test]
    fn an_address_and_a_service_are_two_ways_to_pick_one_advertiser() {
        for bad in [
            serde_json::json!({"addr": "02:00:00", "service": "pk"}),
            serde_json::json!({"addr": "not an address"}),
            serde_json::json!({"service": "not-a-uuid"}),
            serde_json::json!({"mtu": 10}),
            serde_json::json!({"within_ms": 0}),
            serde_json::json!({"within_ms": 9000, "timeout_ms": 1000}),
            serde_json::json!({"disconnect": true, "service": "pk"}),
            serde_json::json!({"peer": "02:00:00:00:00:01"}),
        ] {
            assert_eq!(
                BleConnectArgs::from_json(&bad).expect_err("refused").code,
                E_USAGE,
                "{bad}"
            );
        }
        assert_eq!(
            args(serde_json::json!({"service": "pk"})).peer,
            Peer::Service("pk".to_owned())
        );
        assert_eq!(
            resolve_service("pk").expect("pk"),
            resolve_service(PK_SERVICE).expect("uuid")
        );
    }

    #[test]
    fn the_address_type_comes_from_the_scan_report_when_one_is_known() {
        let address = [0x01, 0x00, 0x00, 0x00, 0x00, 0x02];
        let heard = AdvReport {
            address,
            random: true,
            pdu_type: air::pdu::ADV_IND,
            ..AdvReport::default()
        };
        let parsed = args(serde_json::json!({"addr": "02:00:00:00:00:01"}));
        let (target, report) = target_of(&parsed, std::slice::from_ref(&heard)).expect("a target");
        assert_eq!(
            target,
            Target::Address {
                address,
                random: true
            }
        );
        assert!(report.is_some());
        // With no report and no argument the address is public, which is the peripheral's.
        let (target, report) = target_of(&parsed, &[]).expect("a target");
        assert_eq!(
            target,
            Target::Address {
                address,
                random: false
            }
        );
        assert!(report.is_none());
        // An explicit argument wins over the report.
        let parsed = args(serde_json::json!({"addr": "02:00:00:00:00:01", "random": false}));
        let (target, _) = target_of(&parsed, std::slice::from_ref(&heard)).expect("a target");
        assert_eq!(
            target,
            Target::Address {
                address,
                random: false
            }
        );
    }

    use super::super::ble_scan::tests::advertising;

    #[test]
    fn a_firmware_that_advertises_non_connectably_is_refused_at_once() {
        // The demo (`BLE_GAP_CONN_MODE_NON`) advertises ADV_SCAN_IND.
        for (kind, pdu) in [(2, "ADV_SCAN_IND"), (3, "ADV_NONCONN_IND")] {
            let state = advertising(Some(kind));
            for peer in [
                Peer::Any,
                Peer::Service("pk".to_owned()),
                Peer::Addr("02:00:00:12:34:56".to_owned(), None),
            ] {
                let error = refuse_at_once(&peer, &state).expect("refused before the wait");
                assert_eq!(error.code, E_STATE);
                assert_eq!(error.detail["ble"], why::NON_CONNECTABLE, "{peer:?}");
                assert_eq!(error.detail["pdu"], pdu);
                assert!(
                    error.message.contains(pdu)
                        && error.message.contains("without accepting connections"),
                    "{}",
                    error.message
                );
            }
            // Not the firmware's advertiser, so its wait runs.
            let other = Peer::Addr("02:00:00:00:00:09".to_owned(), None);
            assert!(refuse_at_once(&other, &state).is_none());
        }
        // A connectable firmware, and one that advertises nothing yet, are waited for.
        assert!(refuse_at_once(&Peer::Any, &advertising(Some(0))).is_none());
        assert!(refuse_at_once(&Peer::Any, &advertising(None)).is_none());
    }

    #[test]
    fn a_wait_that_runs_out_names_what_the_air_carried() {
        let parsed = args(serde_json::json!({"addr": "02:00:00:12:34:56"}));
        let report = |pdu_type, at_us| AdvReport {
            address: [0x56, 0x34, 0x12, 0x00, 0x00, 0x02],
            pdu_type,
            at_us,
            ..AdvReport::default()
        };
        let mut state = advertising(None);
        // A report from before the wait does not count.
        state.central.reports = vec![report(air::pdu::ADV_IND, 10)];
        let quiet = wait_ran_out(&parsed, &state, 100);
        assert_eq!(quiet.detail["ble"], why::NOT_ADVERTISING, "{quiet:?}");
        // Heard, but not the named address.
        let mut elsewhere = report(air::pdu::ADV_IND, 200);
        elsewhere.address = [9, 0, 0, 0, 0, 2];
        state.central.reports = vec![elsewhere];
        let missing = wait_ran_out(&parsed, &state, 100);
        assert_eq!(missing.detail["ble"], why::PEER_NOT_SEEN, "{missing:?}");
        assert!(
            missing.message.contains("02:00:00:00:00:09"),
            "{}",
            missing.message
        );
        // Heard only as a scannable advertiser.
        state.central.reports = vec![report(air::pdu::ADV_SCAN_IND, 200)];
        let refused = wait_ran_out(&parsed, &state, 100);
        assert_eq!(refused.detail["ble"], why::NON_CONNECTABLE, "{refused:?}");
        assert_eq!(refused.detail["pdu"], "ADV_SCAN_IND");
        // The timeout path, which a retry can clear.
        state.central.reports = vec![report(air::pdu::ADV_IND, 200)];
        let slow = wait_ran_out(&parsed, &state, 100);
        assert_eq!(slow.detail["ble"], why::NO_ANSWER, "{slow:?}");
        assert!(slow.retryable);
        assert!(slow.message.contains("2000 ms"), "{}", slow.message);
    }

    #[test]
    fn the_command_is_in_the_radio_group_and_its_examples_parse() {
        let spec = crate::registry::find("ble_connect").expect("#[command] registered it");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let value: serde_json::Value =
                serde_json::from_str(example.args).expect("the example is JSON");
            BleConnectArgs::from_json(&value).expect("every example parses");
        }
    }
}
