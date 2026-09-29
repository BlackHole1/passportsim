//! `passportsim ble_scan`: the scripted BLE central scans the virtual air. This file also holds
//! what `ble_connect` and `ble_gatt` share.
//!
//! Each of the three commands ([`run_steps`]):
//!
//! 1. journals a `pemu_radio::ble::central::Step` list as one `EnvChange::BleCentral` input at the
//!    current instant, so a replay makes the same world;
//! 2. runs the machine in slices until every step has a result, or `timeout_ms` or the wall budget
//!    runs out, holding the clock lease like `run`;
//! 3. reads the answers out of the bound `ble` module's state (`MachineApi::radio_module_state`).
//!
//! There is no RSSI: the virtual air has no path loss, so a signal strength would be invented. An
//! advertiser address derived from a device eFuse is a `SecretSet` member and is masked; an address
//! the caller supplied is never redacted.

use pemu_core::input::{EnvChange, InputEvent};
use pemu_core::time::VTime;
use pemu_machine::machine::At;
use pemu_radio::ble::air;
use pemu_radio::ble::central::{self, AdvReport, Step, StepResult, StepStatus, Uuid};
use pemu_radio::ble::vhci::BleState;

use crate::error::{
    ApiError, E_DEADLOCK, E_GUEST_PANIC, E_HLE, E_INTERNAL, E_LEASE, E_STATE, E_STUCK, E_TIMEOUT,
    E_TRIPWIRE, E_USAGE, E_WALL_BUDGET,
};
use crate::output::Output;
use crate::registry::command;
use crate::shape::ShapeLimits;
use crate::spec::{HandlerCx, Schema};

use super::nfc_tap::bind_checked;
use super::run::WallBudget;
use crate::args::{instance_schema, object, only, opt_bool, opt_str, opt_u64, usage};
use crate::session::{Session, fault_of, task_deadlock_fault};

pub const MODULE: &str = "ble";

pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
/// Past any GATT procedure and the 30 s ATT transaction timeout (Core Vol 3 Part F 3.3.3).
pub const TIMEOUT_MS_MAX: u64 = 60_000;
/// Shared with the web card (`web/src/app/panels/ble.ts`): long enough to hear an advertiser at any
/// interval the air produces, short enough for the default timeout. Class C.
pub const DEFAULT_SCAN_MS: u64 = 2_000;
pub const SCAN_MS_MAX: u64 = 30_000;
pub const DEFAULT_WALL_BUDGET_MS: u64 = 30_000;

/// The words a refusal carries as `detail.ble`, so the web card can tell states apart without
/// parsing the message.
pub mod why {
    /// `detail.binding` holds the binding word.
    pub const NOT_BOUND: &str = "not_bound";
    /// The firmware has not started its controller this boot.
    pub const NOT_STARTED: &str = "not_started";
    /// The firmware started its controller this boot and took it down again.
    pub const STOPPED: &str = "stopped";
    /// `detail.pdu` names the PDU.
    pub const NON_CONNECTABLE: &str = "non_connectable";
    pub const NOT_ADVERTISING: &str = "not_advertising";
    pub const PEER_NOT_SEEN: &str = "peer_not_seen";
    /// A connectable advertiser was heard and the connection still did not come up.
    pub const NO_ANSWER: &str = "no_answer";
    pub const NOT_CONNECTED: &str = "not_connected";
}

/// The demo's BLE page is the one firmware the page ships, so it is named.
const START_BLUETOOTH: &str = "open Bluetooth on the device first (the demo's BLE page: DOWN five \
                               times from Display, then OK), then scan again";

/// `None` from the machine means no handler of the module ran since boot: none is bound, or the
/// firmware has not called its controller yet. Either is `E_STATE`, because a scan that quietly
/// found nothing would read like a working radio.
pub fn ble_state(session: &mut Session) -> Result<BleState, ApiError> {
    let decoded = match session.machine().radio_module_state(MODULE) {
        Some(bytes) => BleState::decode(bytes),
        None => return Err(absent(&Binding::of(&session.receipt()))),
    };
    decoded.map_err(|err| {
        ApiError::new(
            E_INTERNAL,
            format!("the BLE module state does not decode: {err:?}"),
        )
    })
}

/// Also refused while the controller is not running: nothing is on the air.
pub fn controller_up(session: &mut Session) -> Result<BleState, ApiError> {
    let state = ble_state(session)?;
    match not_running(&state) {
        Some(error) => Err(error),
        None => Ok(state),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Binding {
    /// `bound`, `unsupported image`, `disabled` or `not linked`; `None` when the record names no
    /// BLE feature.
    pub word: Option<String>,
    pub elf: bool,
}

impl Binding {
    pub fn of(receipt: &crate::receipt::Receipt) -> Binding {
        let record = receipt.extra.get("binding");
        Binding {
            word: record
                .and_then(|r| r["features"][MODULE].as_str())
                .map(str::to_owned),
            elf: record.is_some_and(|r| !r["app_elf_sha256"].is_null()),
        }
    }
}

pub fn absent(binding: &Binding) -> ApiError {
    if binding.word.as_deref() == Some("bound") {
        return ApiError::new(
            E_STATE,
            "the BLE module is bound, but the firmware has not started its Bluetooth controller \
             yet, so nothing is on the air",
        )
        .with_hint(START_BLUETOOTH)
        .with_detail(serde_json::json!({ "ble": why::NOT_STARTED, "binding": "bound" }));
    }
    let reason = match binding.word.as_deref() {
        Some("not linked") => "the firmware links no Bluetooth controller".to_owned(),
        Some("unsupported image") if !binding.elf => {
            "the image came without its ELF, and its Bluetooth functions could not be recovered \
             from the image's own bytes"
                .to_owned()
        }
        Some("unsupported image") => "the firmware's Bluetooth controller functions do not match \
                                      the profile this emulator binds, so the binding was refused"
            .to_owned(),
        Some("disabled") => "the BLE model is turned off for this instance".to_owned(),
        Some(other) => format!("the binding record says `{other}`"),
        None => "the binding record names no BLE feature".to_owned(),
    };
    ApiError::new(
        E_STATE,
        format!("this instance has no bound BLE module, so it has no virtual central: {reason}"),
    )
    .with_hint(
        "`inspect fidelity` reports the binding: an image that links no controller, a refused \
         binding and a disabled model all leave the radio absent",
    )
    .with_detail(serde_json::json!({
        "ble": why::NOT_BOUND,
        "binding": binding.word,
        "elf": binding.elf,
    }))
}

/// `esp_bt_controller_init` creates the worker and `_deinit` deletes it, so a zero worker is an
/// uninitialized controller. The `enable` lines print once per boot, which tells a stopped
/// controller from one never started.
pub fn not_running(state: &BleState) -> Option<ApiError> {
    if state.worker != 0 {
        return None;
    }
    if !state.phy_logged {
        return Some(absent(&Binding {
            word: Some("bound".to_owned()),
            elf: true,
        }));
    }
    Some(
        ApiError::new(
            E_STATE,
            "the firmware has stopped its Bluetooth controller, so nothing is on the air",
        )
        .with_hint(START_BLUETOOTH)
        .with_detail(serde_json::json!({ "ble": why::STOPPED })),
    )
}

/// From the parameters the guest last set; `None` while advertising is off. The public address is
/// the reverse of the `bt_mac` init read.
pub fn on_air(state: &BleState) -> Option<air::AdvertisingPdu> {
    let mut public = state.bt_mac;
    public.reverse();
    air::advertising_pdu(&state.controller, public)
}

/// `state` is `connected`, `advertising` or `idle`.
pub fn radio_json(state: &BleState) -> serde_json::Value {
    let advertising = on_air(state);
    let word = if state.central.link.is_some() {
        "connected"
    } else if advertising.is_some() {
        "advertising"
    } else {
        "idle"
    };
    serde_json::json!({
        "state": word,
        "advertising": advertising.map(|adv| serde_json::json!({
            "addr": addr_text(adv.adv_a),
            "random": adv.random_address,
            "pdu": pdu_name(adv.pdu_type),
            "connectable": connectable(adv.pdu_type),
            "name": central::ad_local_name(&adv.adv_data)
                .or_else(|| central::ad_local_name(&state.controller.advertising.scan_response)),
        })),
    })
}

/// Names the PDU seen.
pub fn non_connectable(addr: &str, pdu_type: u8) -> ApiError {
    let pdu = pdu_name(pdu_type);
    ApiError::new(
        E_STATE,
        format!(
            "{addr} advertises {pdu}: the firmware advertises without accepting connections, so \
             there is nothing to connect to"
        ),
    )
    .with_hint(
        "a central connects only to ADV_IND or ADV_DIRECT_IND (Core Vol 6 Part B 2.3.1); the \
         firmware chooses its advertising type",
    )
    .with_detail(serde_json::json!({ "ble": why::NON_CONNECTABLE, "pdu": pdu, "addr": addr }))
}

fn journal(session: &mut Session, steps: &[Step]) -> Result<(), ApiError> {
    let script = central::encode_script(steps);
    session
        .machine()
        .input(At::Now, InputEvent::Env(EnvChange::BleCentral { script }))
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the BLE central script at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

// The external HCI bridge, reached through `env`. The guest's host-to-controller packets go into a
// window and the peer's come back as journaled `InputEvent::HciPacket` inputs, so the run stays
// replayable. The pacing is forced to wall rate 1 while it is live.

/// Re-exported so a host transport need not name `pemu-radio` (Core Vol 4 Part A 2).
pub use pemu_radio::ble::hci::{H4Stream, h4_packet_len};

/// Installed by a host that can open a socket. Without one (the wasm core) `env` attaches the
/// bridge in the journal alone. Both take the session, because an id names an instance only within
/// its pool.
#[derive(Copy, Clone)]
pub struct BridgeIo {
    /// Returns the URL a peer connects to.
    pub attach: fn(&Session) -> Result<String, String>,
    /// Closing one that is not open is fine: the journal is the truth about whether it is attached.
    pub detach: fn(&Session) -> Result<(), String>,
}

static BRIDGE_IO: std::sync::OnceLock<BridgeIo> = std::sync::OnceLock::new();

/// The first installation wins.
pub fn set_bridge_io(io: BridgeIo) {
    let _ = BRIDGE_IO.set(io);
}

pub fn installed_bridge_io() -> Option<BridgeIo> {
    BRIDGE_IO.get().copied()
}

pub fn bridge_journal(session: &mut Session, attached: bool) -> Result<(), ApiError> {
    session
        .machine()
        .input(
            At::Now,
            InputEvent::Env(EnvChange::BleHciBridge { attached }),
        )
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the BLE bridge change at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

/// Reading is by cursor and never drains, so a transport changes no guest state or state hash.
pub fn bridge_outbound(
    session: &mut Session,
    cursor: u64,
) -> Result<(Vec<Vec<u8>>, u64, u64), ApiError> {
    let state = ble_state(session)?;
    let (packets, next) = state.external.since(cursor);
    Ok((packets, next, state.external.dropped_out))
}

/// With `Origin::Bridge`: a live host peer, so the run is `Replayable` and every receipt says so.
/// `seq` is the transport's stream position; the module counts gaps as `External::lost_in`.
pub fn bridge_inbound(session: &mut Session, seq: u64, data: Vec<u8>) -> Result<(), ApiError> {
    session
        .machine()
        .input_from(
            At::Now,
            pemu_core::journal::Origin::Bridge,
            InputEvent::HciPacket { seq, data },
        )
        .map(|_| ())
        .map_err(|_| {
            ApiError::new(
                E_STATE,
                "the machine refused the bridged HCI packet at the current instant",
            )
            .with_hint("`status` shows the instance's lifecycle state")
        })
}

pub struct Ran {
    /// In script order.
    pub results: Vec<StepResult>,
    pub state: BleState,
    /// Microseconds.
    pub elapsed_us: u64,
}

impl Ran {
    /// The first step that did not end `Ok`, else `Ok`.
    pub fn status(&self) -> StepStatus {
        self.results
            .iter()
            .map(|r| r.status)
            .find(|s| *s != StepStatus::Ok)
            .unwrap_or(StepStatus::Ok)
    }

    pub fn all_ok(&self) -> bool {
        self.status() == StepStatus::Ok
    }
}

/// A call that reaches `timeout` is `E_TIMEOUT` naming the step still running. A guest fault ends
/// the call with its own error.
pub fn run_steps(
    session: &mut Session,
    steps: Vec<Step>,
    timeout: VTime,
    wall_budget_ms: u64,
) -> Result<Ran, ApiError> {
    let base = ble_state(session)?.central.next_index;
    // The module takes a capture step out of the script, so it gets no index or result.
    let want =
        u32::try_from(steps.iter().filter(|s| !is_module_step(s)).count()).unwrap_or(u32::MAX);
    journal(session, &steps)?;
    let started = session.now();
    let deadline = VTime(started.0.saturating_add(timeout.0));
    let budget = WallBudget::start(session, wall_budget_ms);
    let slice = super::run::slice_of(timeout);
    loop {
        let state = ble_state(session)?;
        let results: Vec<StepResult> = state
            .central
            .results
            .iter()
            .filter(|r| r.index >= base && r.index < base.saturating_add(want))
            .cloned()
            .collect();
        if u32::try_from(results.len()).unwrap_or(u32::MAX) >= want {
            let elapsed_us = session.now().as_us().saturating_sub(started.as_us());
            return Ok(Ran {
                results,
                state,
                elapsed_us,
            });
        }
        if session.now().0 >= deadline.0 {
            return Err(timed_out(&state, &steps, results.len(), timeout));
        }
        budget.check(session)?;
        let now = session.now();
        let until = VTime(now.0.saturating_add(slice.0.max(1)).min(deadline.0));
        let outcome = session.run_until(until);
        if let Some(error) = fault_of(&outcome.reason) {
            return Err(error.at_vt_us(outcome.vt.as_us()));
        }
        if let Some(error) = task_deadlock_fault(session) {
            return Err(error);
        }
    }
}

fn is_module_step(step: &Step) -> bool {
    matches!(step, Step::Capture { .. } | Step::CaptureSecrets { .. })
}

fn timed_out(state: &BleState, steps: &[Step], done: usize, timeout: VTime) -> ApiError {
    let stuck = steps
        .iter()
        .filter(|s| !is_module_step(s))
        .nth(done)
        .map_or_else(|| "the script".to_owned(), step_name);
    let air = format!(
        "{} advertising event(s) on the air, {} advertiser(s) heard, {}",
        state.air.adv_events,
        state.central.reports.len(),
        match state.central.link.as_ref() {
            Some(link) => format!("connected with MTU {}", link.mtu),
            None => "not connected".to_owned(),
        }
    );
    ApiError::new(
        E_TIMEOUT,
        format!(
            "the BLE script did not finish within {} ms of virtual time: {stuck} was still running",
            timeout.as_us() / 1_000
        ),
    )
    .retryable()
    .with_hint(air)
}

fn step_name(step: &Step) -> String {
    match step {
        Step::Scan { ms } => format!("scan {ms} ms"),
        Step::Connect { .. } => "connect".to_owned(),
        Step::ExchangeMtu { mtu } => format!("exchange MTU {mtu}"),
        Step::DiscoverServices => "discover services".to_owned(),
        Step::DiscoverCharacteristics { service } => {
            format!("discover the characteristics of {}", uuid_text(*service))
        }
        Step::Subscribe { characteristic, .. } => {
            format!("subscribe to {}", uuid_text(*characteristic))
        }
        Step::Write { characteristic, .. } => format!("write to {}", uuid_text(*characteristic)),
        Step::Read { characteristic } => format!("read {}", uuid_text(*characteristic)),
        Step::WaitNotification { characteristic, .. } => {
            format!("wait for a notification on {}", uuid_text(*characteristic))
        }
        Step::Wait { ms } => format!("wait {ms} ms"),
        Step::Disconnect => "disconnect".to_owned(),
        Step::Capture { enabled } => format!("capture {}", if *enabled { "on" } else { "off" }),
        Step::CaptureSecrets { secrets } => format!(
            "capture {}",
            if *secrets {
                "with key material"
            } else {
                "redacted"
            }
        ),
    }
}

pub fn status_name(status: StepStatus) -> String {
    match status {
        StepStatus::Ok => "ok".to_owned(),
        StepStatus::Timeout => "timeout".to_owned(),
        StepStatus::NotConnected => "not_connected".to_owned(),
        StepStatus::NotFound => "not_found".to_owned(),
        StepStatus::AttError(code) => format!("att_error_{code:#04x}"),
        StepStatus::TooLong => "too_long".to_owned(),
        StepStatus::Disconnected => "disconnected".to_owned(),
    }
}

/// So a failed exchange is an error rather than a success whose JSON has to be read.
pub fn step_refused(what: &str, status: StepStatus) -> ApiError {
    let detail = match status {
        StepStatus::NotConnected => "the central is not connected".to_owned(),
        StepStatus::NotFound => "the central knows no such service or characteristic".to_owned(),
        StepStatus::AttError(code) => format!("the server answered ATT error {code:#04x}"),
        StepStatus::TooLong => "the value does not fit the ATT MTU".to_owned(),
        StepStatus::Disconnected => "the connection ended during the step".to_owned(),
        StepStatus::Timeout => "the step ran out of time".to_owned(),
        StepStatus::Ok => "the step succeeded".to_owned(),
    };
    let error = ApiError::new(E_STATE, format!("{what} did not complete: {detail}")).with_hint(
        "`ble_gatt --op discover` lists what the central knows, and `ble_scan` shows the air",
    );
    match status {
        StepStatus::NotConnected => {
            error.with_detail(serde_json::json!({ "ble": why::NOT_CONNECTED }))
        }
        _ => error,
    }
}

/// Names what is on the air, so the reader knows whether connecting can help. Comes before anything
/// is journaled.
pub fn not_connected(what: &str, state: &BleState) -> ApiError {
    match on_air(state) {
        Some(adv) if !connectable(adv.pdu_type) => {
            let pdu = pdu_name(adv.pdu_type);
            let addr = addr_text(adv.adv_a);
            ApiError::new(
                E_STATE,
                format!(
                    "{what} needs a connection, and {addr} advertises {pdu}: the firmware \
                     advertises without accepting connections, so there is no GATT server to reach"
                ),
            )
            .with_hint("a firmware that advertises ADV_IND accepts a central")
            .with_detail(
                serde_json::json!({ "ble": why::NON_CONNECTABLE, "pdu": pdu, "addr": addr }),
            )
        }
        adv => ApiError::new(
            E_STATE,
            format!(
                "{what} needs a connection, and the virtual central is not connected{}",
                match &adv {
                    Some(adv) => format!(
                        "; {} advertises {}, connectable",
                        addr_text(adv.adv_a),
                        pdu_name(adv.pdu_type)
                    ),
                    None => "; nothing is advertising now".to_owned(),
                }
            ),
        )
        .with_hint("`ble_connect` opens a connection to a connectable advertiser")
        .with_detail(serde_json::json!({
            "ble": why::NOT_CONNECTED,
            "advertising": adv.is_some(),
        })),
    }
}

/// Most significant octet first, colon separated, upper case. The air carries it least significant
/// first (Core Vol 6 Part B 2.3).
pub fn addr_text(address: [u8; 6]) -> String {
    let mut octets = address;
    octets.reverse();
    octets
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// `None` unless six colon- or dash-separated hex octets.
pub fn parse_addr(text: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = text.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (slot, part) in out.iter_mut().zip(parts) {
        if part.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(part, 16).ok()?;
    }
    out.reverse();
    Some(out)
}

pub use pemu_loader::hex as hex_text;

pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = text.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if digits.is_empty() || !digits.len().is_multiple_of(2) {
        return None;
    }
    digits
        .chunks(2)
        .map(|pair| {
            core::str::from_utf8(pair)
                .ok()
                .and_then(|s| u8::from_str_radix(s, 16).ok())
        })
        .collect()
}

/// Canonical form, upper case.
pub fn uuid_text(uuid: Uuid) -> String {
    let mut b = uuid.0;
    b.reverse();
    let hex = hex_text(&b).to_uppercase();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The canonical form, or a 4-digit 16-bit UUID.
pub fn parse_uuid(text: &str, at: &str) -> Result<Uuid, ApiError> {
    Uuid::parse(text).ok_or_else(|| {
        usage(
            at,
            &format!(
                "`{text}` is not a UUID: write 12D4FA08-7418-48FA-A95A-B43A2E669E55 or a 16-bit \
                 UUID like 2A00"
            ),
        )
    })
}

pub fn pdu_name(pdu_type: u8) -> &'static str {
    match pdu_type {
        air::pdu::ADV_IND => "ADV_IND",
        air::pdu::ADV_DIRECT_IND => "ADV_DIRECT_IND",
        air::pdu::ADV_NONCONN_IND => "ADV_NONCONN_IND",
        air::pdu::SCAN_REQ => "SCAN_REQ",
        air::pdu::SCAN_RSP => "SCAN_RSP",
        air::pdu::CONNECT_IND => "CONNECT_IND",
        air::pdu::ADV_SCAN_IND => "ADV_SCAN_IND",
        _ => "unknown",
    }
}

/// Core Vol 6 Part B 2.3.1.
pub fn connectable(pdu_type: u8) -> bool {
    matches!(pdu_type, air::pdu::ADV_IND | air::pdu::ADV_DIRECT_IND)
}

/// From the advertising data or the scan response.
pub fn report_name(report: &AdvReport) -> Option<String> {
    central::ad_local_name(&report.data).or_else(|| {
        report
            .scan_response
            .as_deref()
            .and_then(central::ad_local_name)
    })
}

pub fn report_json(report: &AdvReport) -> serde_json::Value {
    serde_json::json!({
        "addr": addr_text(report.address),
        "random": report.random,
        "name": report_name(report),
        "pdu": pdu_name(report.pdu_type),
        "connectable": connectable(report.pdu_type),
        "channel": report.channel,
        "at_us": report.at_us,
        // No `rssi`: the air models no path loss.
        "data": hex_text(&report.data),
        "scan_response": report.scan_response.as_deref().map(hex_text),
    })
}

pub fn timeout_of(args: &serde_json::Map<String, serde_json::Value>) -> Result<VTime, ApiError> {
    let ms = opt_u64(args, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS);
    if ms == 0 || ms > TIMEOUT_MS_MAX {
        return Err(usage(
            "timeout_ms",
            &format!("a timeout is between 1 and {TIMEOUT_MS_MAX} ms of virtual time"),
        ));
    }
    Ok(VTime::from_ms(ms))
}

pub fn wall_budget_of(args: &serde_json::Map<String, serde_json::Value>) -> Result<u64, ApiError> {
    let ms = opt_u64(args, "wall_budget_ms")?.unwrap_or(DEFAULT_WALL_BUDGET_MS);
    if ms == 0 {
        return Err(usage("wall_budget_ms", "a wall budget is at least 1 ms"));
    }
    Ok(ms)
}

pub fn common_properties() -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("instance".into(), instance_schema());
    map.insert(
        "timeout_ms".into(),
        serde_json::json!({
            "type": "integer",
            "minimum": 1,
            "maximum": TIMEOUT_MS_MAX,
            "description": "Virtual time this call may spend before E_TIMEOUT."
        }),
    );
    map.insert(
        "wall_budget_ms".into(),
        serde_json::json!({
            "type": "integer",
            "minimum": 1,
            "description": "Host time this call may spend before E_WALL_BUDGET."
        }),
    );
    map
}

pub fn common_json(
    instance: &str,
    ran: &Ran,
    receipt: &crate::receipt::Receipt,
) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("instance".into(), instance.into());
    map.insert("vt_us".into(), receipt.vt_us.into());
    map.insert("elapsed_vt_us".into(), ran.elapsed_us.into());
    map.insert("status".into(), status_name(ran.status()).into());
    map.insert(
        "steps".into(),
        serde_json::Value::Array(
            ran.results
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "index": r.index,
                        "at_us": r.at_us,
                        "status": status_name(r.status),
                    })
                })
                .collect(),
        ),
    );
    map.insert("radio".into(), radio_json(&ran.state));
    map
}

/// The Passport Keys vendor service (`pk_ble.c`), short name `pk`. The firmware's own UUID, neither
/// a secret nor an address.
pub const PK_SERVICE: &str = "12D4FA08-7418-48FA-A95A-B43A2E669E55";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleScanArgs {
    pub instance: Option<String>,
    pub duration_ms: u64,
    /// Only the advertisers this scan added.
    pub fresh: bool,
    pub timeout: VTime,
    pub wall_budget_ms: u64,
}

impl BleScanArgs {
    pub fn from_json(value: &serde_json::Value) -> Result<BleScanArgs, ApiError> {
        let args = object(value)?;
        only(
            args,
            &[
                "instance",
                "duration_ms",
                "fresh",
                "timeout_ms",
                "wall_budget_ms",
            ],
        )?;
        let duration_ms = opt_u64(args, "duration_ms")?.unwrap_or(DEFAULT_SCAN_MS);
        if duration_ms > SCAN_MS_MAX {
            return Err(usage(
                "duration_ms",
                &format!("a scan is between 0 (no scan) and {SCAN_MS_MAX} ms"),
            ));
        }
        let timeout = timeout_of(args)?;
        if VTime::from_ms(duration_ms).0 > timeout.0 {
            return Err(usage(
                "duration_ms",
                "the scan is longer than the call's timeout_ms",
            ));
        }
        Ok(BleScanArgs {
            instance: opt_str(args, "instance")?.map(str::to_owned),
            duration_ms,
            fresh: opt_bool(args, "fresh")?.unwrap_or(false),
            timeout,
            wall_budget_ms: wall_budget_of(args)?,
        })
    }
}

/// A `duration_ms` of 0 journals nothing and advances no time: it reports what the central already
/// knows, which is how the web card reads the radio without adding an input.
pub fn ble_scan_on(session: &mut Session, args: &BleScanArgs) -> Result<Output, ApiError> {
    let instance = session.id.to_string();
    let before = controller_up(session)?;
    if args.duration_ms == 0 {
        let ran = Ran {
            results: Vec::new(),
            state: before,
            elapsed_us: 0,
        };
        return Ok(scan_output(session, &instance, args, &ran, None));
    }
    let started_us = session.now().as_us();
    let ran = run_steps(
        session,
        vec![Step::Scan {
            ms: u32::try_from(args.duration_ms).unwrap_or(u32::MAX),
        }],
        args.timeout,
        args.wall_budget_ms,
    )?;
    let scan = Scanned {
        started_us,
        known_before: before.central.reports.len(),
        adv_events: ran
            .state
            .air
            .adv_events
            .saturating_sub(before.air.adv_events),
    };
    Ok(scan_output(session, &instance, args, &ran, Some(&scan)))
}

struct Scanned {
    /// A report at or after it was heard by this scan.
    started_us: u64,
    known_before: usize,
    adv_events: u64,
}

/// `heard`, `silent` (the air carried no advertising event), `none_matching`, or `read` for a scan
/// of nothing.
fn outcome(scan: Option<&Scanned>, heard: usize, listed: usize) -> &'static str {
    match scan {
        None => "read",
        Some(_) if heard > 0 && listed > 0 => "heard",
        Some(s) if s.adv_events == 0 => "silent",
        Some(_) => "none_matching",
    }
}

fn scan_output(
    session: &mut Session,
    instance: &str,
    args: &BleScanArgs,
    ran: &Ran,
    scan: Option<&Scanned>,
) -> Output {
    // The central keeps what it heard across steps and refreshes a report on each hearing. `heard`
    // marks this scan's, so a stopped advertiser is not mistaken for one on the air; `fresh`
    // reports only what this scan added.
    let all = &ran.state.central.reports;
    let heard_now = |r: &AdvReport| scan.is_some_and(|s| r.at_us >= s.started_us);
    let reports: Vec<&AdvReport> = match scan {
        Some(s) if args.fresh => all.iter().skip(s.known_before.min(all.len())).collect(),
        None if args.fresh => Vec::new(),
        _ => all.iter().collect(),
    };
    let heard = all.iter().filter(|r| heard_now(r)).count();
    let found: Vec<serde_json::Value> = reports
        .iter()
        .map(|r| {
            let mut json = report_json(r);
            json["heard"] = heard_now(r).into();
            json
        })
        .collect();
    let word = outcome(scan, heard, reports.len());
    let receipt = session.receipt();
    let mut json = common_json(instance, ran, &receipt);
    json.insert("duration_ms".into(), args.duration_ms.into());
    json.insert("found".into(), serde_json::Value::Array(found));
    json.insert("heard".into(), heard.into());
    json.insert("outcome".into(), word.into());
    json.insert(
        "adv_events".into(),
        scan.map_or(serde_json::Value::Null, |s| s.adv_events.into()),
    );
    let mut text = match scan {
        None => format!(
            "{instance} ble_scan: no scan, {} advertiser(s) known",
            reports.len()
        ),
        Some(s) => format!(
            "{instance} ble_scan {} ms: {} advertiser(s) listed, {heard} heard, {} advertising \
             event(s) on the air",
            args.duration_ms,
            reports.len(),
            s.adv_events
        ),
    };
    for report in &reports {
        text.push_str(&format!(
            "\n  {} {} {}{}{}",
            addr_text(report.address),
            report_name(report).unwrap_or_else(|| "(no name)".to_owned()),
            pdu_name(report.pdu_type),
            if connectable(report.pdu_type) {
                " connectable"
            } else {
                " not connectable"
            },
            if scan.is_some() && !heard_now(report) {
                " (not heard by this scan)"
            } else {
                ""
            }
        ));
    }
    text.push_str(&format!("\n  {}", radio_text(&ran.state)));
    if word == "silent" {
        text.push_str("\n  nothing was on the air: the controller runs and advertised nothing");
    }
    if !ran.all_ok() {
        text.push_str(&format!("\n  the scan ended {}", status_name(ran.status())));
    }
    Output::new(serde_json::Value::Object(json), text, receipt).shaped(&ShapeLimits::DEFAULT)
}

pub fn radio_text(state: &BleState) -> String {
    let link = if state.central.link.is_some() {
        "; the central is connected"
    } else {
        ""
    };
    match on_air(state) {
        None => format!("the firmware advertises nothing now{link}"),
        Some(adv) => format!(
            "the firmware advertises {} as {}, {}{link}",
            pdu_name(adv.pdu_type),
            addr_text(adv.adv_a),
            if connectable(adv.pdu_type) {
                "connectable"
            } else {
                "accepting no connection"
            }
        ),
    }
}

pub fn input_schema() -> Schema {
    let mut properties = common_properties();
    properties.insert(
        "duration_ms".into(),
        serde_json::json!({
            "type": "integer",
            "minimum": 0,
            "maximum": SCAN_MS_MAX,
            "description": "How long the central scans, in milliseconds of virtual time; 0 scans nothing and reports the radio's state."
        }),
    );
    properties.insert(
        "fresh".into(),
        serde_json::json!({
            "type": "boolean",
            "description": "Report only the advertisers this scan added, not every one the central knows."
        }),
    );
    schemars::json_schema!({
        "type": "object",
        "additionalProperties": false,
        "description": "`ble_scan` arguments.",
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
            "duration_ms": { "type": "integer" },
            "status": { "type": "string" },
            "steps": { "type": "array", "items": { "type": "object" } },
            "found": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "addr": { "type": "string" },
                        "random": { "type": "boolean" },
                        "name": { "type": ["string", "null"] },
                        "pdu": { "type": "string" },
                        "connectable": { "type": "boolean" },
                        "channel": { "type": "integer" },
                        "at_us": { "type": "integer" },
                        "data": { "type": "string" },
                        "scan_response": { "type": ["string", "null"] },
                        "heard": { "type": "boolean" }
                    }
                }
            },
            "heard": { "type": "integer" },
            "outcome": { "type": "string", "enum": ["heard", "silent", "none_matching", "read"] },
            "adv_events": { "type": ["integer", "null"] },
            "radio": radio_schema()
        }
    })
}

/// Shared by `ble_connect` and `ble_gatt`.
pub fn radio_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "state": { "type": "string", "enum": ["connected", "advertising", "idle"] },
            "advertising": {
                "type": ["object", "null"],
                "properties": {
                    "addr": { "type": "string" },
                    "random": { "type": "boolean" },
                    "pdu": { "type": "string" },
                    "connectable": { "type": "boolean" },
                    "name": { "type": ["string", "null"] }
                }
            }
        }
    })
}

/// Scan the virtual air with the scripted BLE central.
#[command(
    api_crate = crate,
    name = "ble_scan",
    group = radio,
    input_schema = input_schema,
    output_schema = output_schema,
    annotations(advances_time, needs_instance),
    cli(positional = ["duration_ms"]),
    scenario_step = "ble.scan",
    errors(E_USAGE, E_STATE, E_LEASE, E_TIMEOUT, E_WALL_BUDGET, E_GUEST_PANIC, E_DEADLOCK, E_STUCK, E_TRIPWIRE, E_HLE, E_INTERNAL),
    example(
        title = "Listen for two seconds and list what advertises",
        args = r#"{"duration_ms":2000}"#,
    ),
    example(
        title = "Report the radio's state without scanning or advancing time",
        args = r#"{"duration_ms":0}"#,
    ),
    example(
        title = "A short sweep reporting only what it newly heard",
        args = r#"{"duration_ms":200,"fresh":true}"#,
    ),
)]
pub fn ble_scan(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
    let args = BleScanArgs::from_json(&args)?;
    crate::pool::with_session(
        |pool| bind_checked(pool, SPEC_BLE_SCAN.annotations, args.instance.as_deref()),
        |session| ble_scan_on(session, &args),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn an_address_round_trips_through_its_display_form() {
        // Least significant octet first on the air, most significant first on the screen.
        let air = [0x01, 0x00, 0x00, 0x00, 0x00, 0x02];
        assert_eq!(addr_text(air), "02:00:00:00:00:01");
        assert_eq!(parse_addr("02:00:00:00:00:01"), Some(air));
        assert_eq!(parse_addr("02-00-00-00-00-01"), Some(air));
        assert_eq!(parse_addr("02:00:00:00:01"), None);
        assert_eq!(parse_addr("nonsense"), None);
    }

    #[test]
    fn a_uuid_round_trips_through_its_display_form() {
        let uuid = parse_uuid(PK_SERVICE, "service").expect("the vendor service");
        assert_eq!(uuid_text(uuid), PK_SERVICE);
        assert_eq!(
            uuid_text(parse_uuid("2A00", "uuid").expect("a 16-bit UUID")),
            "00002A00-0000-1000-8000-00805F9B34FB"
        );
        assert_eq!(parse_uuid("not-a-uuid", "uuid").unwrap_err().code, E_USAGE);
    }

    #[test]
    fn hex_round_trips_and_refuses_an_odd_string() {
        assert_eq!(hex_text(&[0xDE, 0xAD]), "dead");
        assert_eq!(parse_hex("DEad"), Some(vec![0xDE, 0xAD]));
        assert_eq!(parse_hex("abc"), None);
        assert_eq!(parse_hex(""), None);
    }

    #[test]
    fn the_arguments_are_checked_before_any_machine_is_bound() {
        for bad in [
            serde_json::json!({"duration_ms": SCAN_MS_MAX + 1}),
            serde_json::json!({"duration_ms": 9000, "timeout_ms": 1000}),
            serde_json::json!({"timeout_ms": 0}),
            serde_json::json!({"nonsense": true}),
        ] {
            assert_eq!(
                BleScanArgs::from_json(&bad).expect_err("refused").code,
                E_USAGE,
                "{bad}"
            );
        }
        let ok = BleScanArgs::from_json(&serde_json::json!({})).expect("the defaults");
        assert_eq!(ok.duration_ms, DEFAULT_SCAN_MS);
        assert_eq!(ok.timeout, VTime::from_ms(DEFAULT_TIMEOUT_MS));
        assert!(!ok.fresh);
        // 0 is the read that scans nothing.
        let read = BleScanArgs::from_json(&serde_json::json!({"duration_ms": 0})).expect("a read");
        assert_eq!(read.duration_ms, 0);
    }

    #[test]
    fn the_command_is_in_the_radio_group_and_its_examples_parse() {
        let spec = crate::registry::find("ble_scan").expect("#[command] registered ble_scan");
        assert_eq!(spec.group, crate::spec::CapsGroup::Radio);
        for example in spec.examples {
            let args: serde_json::Value =
                serde_json::from_str(example.args).expect("the example is JSON");
            BleScanArgs::from_json(&args).expect("every example parses");
        }
    }

    /// A real `Machine` with no app ELF binds no radio module; a double cannot stand in for that.
    #[test]
    fn a_machine_with_no_bound_module_refuses_by_naming_the_binding() {
        let (mut pool, id) = super::super::nfc_tap::tests::real_pool();
        let session = pool.session_mut(id).expect("a session");
        let error = ble_state(session).expect_err("no module is bound");
        assert_eq!(error.code, E_STATE);
        assert!(
            error.message.contains("BLE module"),
            "the refusal must name the binding: {}",
            error.message
        );
        // The erased image links nothing, so the refusal is "not bound", never "not started".
        assert_eq!(error.detail["ble"], why::NOT_BOUND, "{error:?}");
        let binding = Binding::of(&session.receipt());
        assert_ne!(binding.word.as_deref(), Some("bound"));
        let args = BleScanArgs::from_json(&serde_json::json!({})).expect("the defaults");
        let scan = ble_scan_on(session, &args).expect_err("refused");
        assert_eq!(scan.code, E_STATE);
        assert_eq!(scan.detail["ble"], why::NOT_BOUND);
        let read = BleScanArgs::from_json(&serde_json::json!({"duration_ms": 0})).expect("a read");
        assert_eq!(
            ble_scan_on(session, &read).expect_err("refused").detail["ble"],
            why::NOT_BOUND
        );
    }

    /// `kind` is the `Advertising_Type`; `None` advertises nothing.
    pub(crate) fn advertising(kind: Option<u8>) -> BleState {
        let mut state = BleState {
            worker: 0x3FC9_0000,
            phy_logged: true,
            bt_mac: [0x02, 0x00, 0x00, 0x12, 0x34, 0x56],
            ..BleState::default()
        };
        if let Some(kind) = kind {
            state.controller.advertising.enabled = true;
            state.controller.advertising.params[4] = kind;
            state.controller.advertising.data =
                parse_hex("0201060d09466f6c6f50617373706f7274").expect("hex");
        }
        state
    }

    #[test]
    fn a_missing_module_state_is_told_apart_by_the_binding_word() {
        let started = absent(&Binding {
            word: Some("bound".to_owned()),
            elf: false,
        });
        assert_eq!(started.code, E_STATE);
        assert_eq!(started.detail["ble"], why::NOT_STARTED);
        assert!(
            started
                .message
                .contains("has not started its Bluetooth controller"),
            "{}",
            started.message
        );
        assert!(
            !started.message.contains("no bound BLE module"),
            "a bound module is never called unbound: {}",
            started.message
        );
        assert!(
            started
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("BLE page")),
            "the refusal says what to do: {started:?}"
        );
        for (word, elf, reason) in [
            (Some("not linked"), true, "links no Bluetooth controller"),
            (Some("unsupported image"), false, "came without its ELF"),
            (Some("unsupported image"), true, "binding was refused"),
            (Some("disabled"), true, "turned off"),
            (None, true, "names no BLE feature"),
        ] {
            let error = absent(&Binding {
                word: word.map(str::to_owned),
                elf,
            });
            assert_eq!(error.detail["ble"], why::NOT_BOUND, "{word:?}");
            assert_eq!(error.detail["binding"], serde_json::json!(word), "{word:?}");
            assert!(
                error.message.contains("no bound BLE module") && error.message.contains(reason),
                "{word:?} {elf}: {}",
                error.message
            );
        }
    }

    #[test]
    fn a_controller_that_is_not_running_is_not_started_or_stopped() {
        let mut state = advertising(None);
        assert!(
            not_running(&state).is_none(),
            "a worker is a running controller"
        );
        state.worker = 0;
        assert_eq!(
            not_running(&state).expect("refused").detail["ble"],
            why::STOPPED,
            "it printed its enable lines this boot, so the firmware stopped it"
        );
        state.phy_logged = false;
        assert_eq!(
            not_running(&state).expect("refused").detail["ble"],
            why::NOT_STARTED
        );
    }

    #[test]
    fn the_radio_reports_the_advertising_the_firmware_set() {
        let idle = radio_json(&advertising(None));
        assert_eq!(idle["state"], "idle");
        assert!(idle["advertising"].is_null());
        // The demo's non-connectable mode with a scan response is `ADV_SCAN_IND`.
        let demo = radio_json(&advertising(Some(2)));
        assert_eq!(demo["state"], "advertising");
        assert_eq!(demo["advertising"]["pdu"], "ADV_SCAN_IND");
        assert_eq!(demo["advertising"]["connectable"], false);
        assert_eq!(demo["advertising"]["name"], "FoloPassport");
        assert_eq!(demo["advertising"]["addr"], "02:00:00:12:34:56");
        assert_eq!(
            radio_json(&advertising(Some(3)))["advertising"]["pdu"],
            "ADV_NONCONN_IND"
        );
        assert_eq!(
            radio_json(&advertising(Some(0)))["advertising"]["connectable"],
            true
        );
    }

    #[test]
    fn every_answer_carries_the_radio_state() {
        let ran = Ran {
            results: Vec::new(),
            state: advertising(Some(2)),
            elapsed_us: 0,
        };
        let json = common_json("p1", &ran, &crate::receipt::Receipt::default());
        assert_eq!(json["radio"]["advertising"]["pdu"], "ADV_SCAN_IND");
    }

    #[test]
    fn a_gatt_step_without_a_link_says_whether_connecting_can_help() {
        let demo = not_connected("the service discovery", &advertising(Some(2)));
        assert_eq!(demo.detail["ble"], why::NON_CONNECTABLE);
        assert_eq!(demo.detail["pdu"], "ADV_SCAN_IND");
        assert!(
            demo.message.contains("without accepting connections"),
            "{}",
            demo.message
        );
        let pk = not_connected("the service discovery", &advertising(Some(0)));
        assert_eq!(pk.detail["ble"], why::NOT_CONNECTED);
        assert_eq!(pk.detail["advertising"], true);
        let quiet = not_connected("the read", &advertising(None));
        assert_eq!(quiet.detail["ble"], why::NOT_CONNECTED);
        assert_eq!(quiet.detail["advertising"], false);
    }

    #[test]
    fn a_scan_outcome_tells_a_silent_air_from_an_empty_list() {
        let scan = |adv_events| Scanned {
            started_us: 0,
            known_before: 0,
            adv_events,
        };
        assert_eq!(outcome(None, 0, 0), "read");
        assert_eq!(outcome(Some(&scan(0)), 0, 0), "silent");
        assert_eq!(outcome(Some(&scan(12)), 1, 1), "heard");
        assert_eq!(outcome(Some(&scan(12)), 1, 0), "none_matching");
        assert_eq!(outcome(Some(&scan(12)), 0, 1), "none_matching");
    }

    #[test]
    fn a_local_name_is_read_out_of_the_advertising_data() {
        // The `FoloPassport` advertising data: flags, then a complete local name.
        let data = parse_hex("0201060d09466f6c6f50617373706f7274").expect("hex");
        let report = AdvReport {
            data,
            pdu_type: air::pdu::ADV_IND,
            ..AdvReport::default()
        };
        let json = report_json(&report);
        assert_eq!(json["name"], "FoloPassport");
        assert_eq!(json["pdu"], "ADV_IND");
        assert_eq!(json["connectable"], true);
    }
}
