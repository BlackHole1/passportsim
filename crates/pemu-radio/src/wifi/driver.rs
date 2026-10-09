//! The Wi-Fi driver state machine and the `wifi` worker, as the `ModuleHost` of the Wi-Fi module.
//!
//! Each handler is the observable contract of the function it replaces, from the public
//! `esp_wifi.h` documentation, never the blob behind it; the scan and association rules are
//! device-measured. A handler never posts an event itself: it queues work in
//! [`WifiState::outbox`] for the worker, which posts events before releasing a parked caller, so
//! a scan dwell is virtual time and a snapshot between a scan and its result carries both.

use std::collections::BTreeMap;

use pemu_core::irq_source::IrqSource;
use pemu_core::snap::{SnapError, SnapReader, SnapValue, snap_struct};
use pemu_core::time::VTime;
use pemu_hle::binding::ImageView;
use pemu_hle::continuation::{HandlerState, Resume};
use pemu_hle::core::{CallInfo, ModuleHost, NetInput, module_timer, worker_handler};
use pemu_hle::guest_call::{A0, Arg, CallRequest, GuestView, HleAction, HleError, HleErrorKind};
use pemu_hle::hooks::{HandlerKind, ModuleIndex};
use pemu_hle::magic::{MagicKind, MagicPcs};
use pemu_hle::worker::{
    PORT_MAX_DELAY, RadioEvent, WakeMode, WakeReason, WorkerCalls, WorkerConfig, wifi_profile,
};

use pemu_hle::log_synth::{LogLevel, LogLineTemplate, LogSynth};

use crate::log_lines::{LogLines, Stage};

use super::ap::ScriptedAp;
use super::profile::WifiProfile;
use crate::heap_ledger::{LEDGER_CAPS, Ledger, LedgerRow};
use crate::hle_common::{self, bad_step, ret, returned, store_words, word_at, words_of};
use crate::lan::gateway::Lan;
use crate::lan::pcap::{self, Capture};

pub const ESP_OK: u32 = 0;
pub const ESP_ERR_NO_MEM: u32 = 0x101;
pub const ESP_ERR_INVALID_ARG: u32 = 0x102;
/// 12289: a Wi-Fi API before `esp_wifi_init`.
pub const ESP_ERR_WIFI_NOT_INIT: u32 = 0x3001;
/// 12290: an API that needs a started driver.
pub const ESP_ERR_WIFI_NOT_STARTED: u32 = 0x3002;
/// 12291: `esp_wifi_deinit` while started.
pub const ESP_ERR_WIFI_NOT_STOPPED: u32 = 0x3003;
/// 12292: an interface that is not `WIFI_IF_STA` or `WIFI_IF_AP`.
pub const ESP_ERR_WIFI_IF: u32 = 0x3004;
/// 12295: the `esp_wifi_internal_tx` answer for an interface the mode did not create
/// (`esp_private/wifi.h`).
pub const ESP_ERR_WIFI_CONN: u32 = 0x3007;
/// 12309: the `esp_wifi_internal_tx` answer for a started station that is not associated.
pub const ESP_ERR_WIFI_NOT_ASSOC: u32 = 0x3015;
/// `WIFI_MODE_AP`; it and `WIFI_MODE_APSTA` create the SoftAP interface.
pub const MODE_AP: u32 = 2;
/// `WIFI_MODE_APSTA` (`esp_wifi_types_generic.h`).
pub const MAX_MODE: u32 = 3;
/// `WIFI_STORAGE_RAM`.
pub const MAX_STORAGE: u32 = 1;
/// `WIFI_IF_AP`.
pub const MAX_IF: u32 = 1;
/// `sizeof(wifi_config_t)`.
pub const CONFIG_BYTES: usize = 184;
pub const ESP_MAC_WIFI_STA: u32 = 0;
/// `ESP_INTR_FLAG_LEVEL1`; see the BLE module for why not `IRAM`.
pub const INTR_FLAG_LEVEL1: u32 = 1 << 1;
pub const QUEUE_SEND_TO_BACK: u32 = 0;

/// The RX buffer row of `wifi.toml`'s `[[heap]]` plan, looked up by label so a new row cannot
/// silently move the capacity the data plane reads.
pub const RX_ROW: &str = "dynamic_rx";

/// Microseconds from a handler queueing work to the worker receiving it. Class C: it exists so
/// the work is never done inside the calling handler, as the BLE controller's `reply_us`.
pub const POST_US: u64 = 500;

mod timer {
    pub const DISPATCH: u16 = 0;
    pub const SCAN: u16 = 1;
    /// The instant an outstanding `esp_wifi_connect` attempt resolves.
    pub const CONNECT: u16 = 2;
}

mod work {
    /// Post a `WIFI_EVENT`: the payload is the 4-byte event id and then the event data.
    pub const POST: u16 = 0;
    /// Release the caller parked on the caller semaphore.
    pub const WAKE: u16 = 1;
    /// Deliver one received frame: the payload is the one-byte interface index and then the
    /// Ethernet frame.
    pub const RX: u16 = 2;
}

pub mod handler {
    pub const INIT: u16 = 0;
    pub const DEINIT: u16 = 1;
    pub const SET_MODE: u16 = 2;
    pub const GET_MODE: u16 = 3;
    pub const SET_STORAGE: u16 = 4;
    pub const SET_CONFIG: u16 = 5;
    pub const GET_CONFIG: u16 = 6;
    pub const GET_MAC: u16 = 7;
    pub const START: u16 = 8;
    pub const STOP: u16 = 9;
    pub const SCAN_START: u16 = 10;
    pub const SCAN_STOP: u16 = 11;
    pub const SCAN_GET_AP_NUM: u16 = 12;
    pub const SCAN_GET_AP_RECORDS: u16 = 13;
    pub const REG_RXCB: u16 = 14;
    pub const REG_NETSTACK_BUF_CB: u16 = 15;
    pub const FREE_RX_BUFFER: u16 = 16;
    /// `esp_wifi_connect_internal`, which `esp_wifi_connect` only tail-calls.
    pub const CONNECT: u16 = 17;
    /// `esp_wifi_disconnect_internal`, likewise for `esp_wifi_disconnect`.
    pub const DISCONNECT: u16 = 18;
    pub const TX: u16 = 19;
    /// `esp_wifi_internal_set_sta_ip`, which `esp_netif`'s `GOT_IP` handler calls.
    pub const SET_STA_IP: u16 = 20;
}

/// `GOT_IP` is deliberately not a driver state: the address comes from lwIP's DHCP client over
/// the data plane, which the driver does not own.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum DriverState {
    #[default]
    Uninit,
    Init,
    /// The station is up and the events flow.
    Started,
    Scanning,
    Connecting,
    /// Associated with [`WifiState::peer`]: `STA_CONNECTED` has been queued.
    Connected,
}

impl DriverState {
    pub fn inited(self) -> bool {
        self != DriverState::Uninit
    }

    /// Started and not stopped, which is what makes `esp_wifi_deinit` return
    /// `ESP_ERR_WIFI_NOT_STOPPED`.
    pub fn started(self) -> bool {
        self >= DriverState::Started
    }
}

impl SnapValue for DriverState {
    fn snap_write(&self, out: &mut Vec<u8>) {
        (*self as u8).snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<DriverState, SnapError> {
        Ok(match u8::snap_read(r)? {
            0 => DriverState::Uninit,
            1 => DriverState::Init,
            2 => DriverState::Started,
            3 => DriverState::Scanning,
            4 => DriverState::Connecting,
            5 => DriverState::Connected,
            _ => {
                return Err(SnapError::Malformed {
                    at: "hle.machine",
                    reason: "unknown wifi driver state",
                });
            }
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub due_us: u64,
    pub tag: u16,
    pub payload: Vec<u8>,
}

snap_struct!(Outgoing {
    due_us,
    tag,
    payload
});

/// Kept as bytes in the machine's `hle.machine` section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiState {
    pub worker: u32,
    pub semaphore: u32,
    /// The semaphore a blocking public API parks its caller on; one caller at a time.
    pub caller_semaphore: u32,
    /// The U4 interrupt handle, 0 in U5.
    pub intr_handle: u32,
    /// The `WIFI_EVENT` base pointer, read from DROM at the first post, 0 before that.
    pub event_base: u32,
    pub mode: u32,
    pub storage: u32,
    pub state: DriverState,
    pub config: Vec<Vec<u8>>,
    pub sta_mac: [u8; 6],
    pub aps: Vec<ScriptedAp>,
    /// The last finished scan's records, strongest first, until `get_ap_records` frees them.
    pub records: Vec<ScriptedAp>,
    pub scan_id: u32,
    pub scan_blocking: bool,
    /// Virtual microseconds.
    pub scan_due_us: u64,
    /// Virtual microseconds; where a partial result is cut.
    pub scan_started_us: u64,
    /// When an outstanding `esp_wifi_connect` attempt resolves, in virtual microseconds, 0 for
    /// none. Kept apart from [`WifiState::state`] because the device runs the attempt's channel
    /// sweep independently of a guest scan: an attempt outstanding while `Scanning` still resolves
    /// on time.
    pub connect_due_us: u64,
    /// The access point being associated with (`Connecting`) or associated (`Connected`), key
    /// dropped. Taken at `esp_wifi_connect`, so a later `env` change does not retarget or end it.
    pub peer: Option<ScriptedAp>,
    pub synthesized_lines: u64,
    /// The task parked on [`WifiState::caller_semaphore`], 0 for none. A second blocking call while
    /// one is parked is refused, since the wake would release the wrong task.
    pub parked_caller: u32,
    pub rxcb: Vec<u32>,
    pub netstack_cb: [u32; 2],
    /// Work waiting for its instant under a module timer, oldest first.
    pub outbox: Vec<Outgoing>,
    /// Work the worker has been given and not yet done, oldest first.
    pub pending: Vec<Outgoing>,
    pub posted_events: u64,
    /// The RX buffers lent to the netstack and not yet given back, each a real `heap_caps_malloc`.
    pub ledger: Ledger,
    pub rx_delivered: u64,
    /// No callback registered, the RX capacity full (the 33rd frame while 32 are out is dropped
    /// unallocated), or the allocator refusing.
    pub rx_dropped: u64,
    pub rx_freed: u64,
    /// The guest's own `dynamic_rx_buf_num` (`CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM`), read by
    /// `esp_wifi_init`; 0 before an init. A guest asking for 0 is refused by name at its first
    /// frame.
    pub rx_capacity: u32,
    /// Addresses `esp_wifi_internal_free_rx_buffer` was called with that this module never lent.
    /// Nothing is freed for them: a silent `heap_caps_free` of a pointer the driver did not
    /// allocate could corrupt the guest heap.
    pub rx_alien_free: u64,
    pub lan: Lan,
    pub capture: Capture,
    pub tx_frames: u64,
    /// Frames `esp_wifi_internal_tx` refused: a stopped driver, a station that is not associated,
    /// or an interface the mode did not create.
    pub tx_refused: u64,
    /// Virtual microseconds of the first `esp_wifi_internal_set_sta_ip`, 0 for none: `esp_netif`'s
    /// `GOT_IP` handler makes that call, so it is when the guest had its address.
    pub sta_ip_us: u64,
}

snap_struct!(WifiState {
    worker,
    semaphore,
    caller_semaphore,
    intr_handle,
    event_base,
    mode,
    storage,
    state,
    config,
    sta_mac,
    aps,
    records,
    scan_id,
    scan_blocking,
    scan_due_us,
    scan_started_us,
    connect_due_us,
    peer,
    synthesized_lines,
    parked_caller,
    rxcb,
    netstack_cb,
    outbox,
    pending,
    posted_events,
    ledger,
    rx_delivered,
    rx_dropped,
    rx_freed,
    rx_capacity,
    rx_alien_free,
    lan,
    capture,
    tx_frames,
    tx_refused,
    sta_ip_us,
});

impl WifiState {
    pub fn decode(bytes: &[u8]) -> Result<WifiState, HleError> {
        if bytes.is_empty() {
            return Ok(WifiState::default());
        }
        let mut r = SnapReader::new(bytes, "hle.machine");
        let state = WifiState::snap_read(&mut r)
            .map_err(|e| HleError::new(HleErrorKind::Handler, format!("wifi state: {e:?}")))?;
        if !r.is_empty() {
            return Err(HleError::new(
                HleErrorKind::Handler,
                "wifi state has trailing bytes",
            ));
        }
        Ok(state)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.snap_write(&mut out);
        out
    }

    pub fn config_of(&self, ifx: usize) -> Vec<u8> {
        self.config
            .get(ifx)
            .cloned()
            .unwrap_or_else(|| vec![0u8; CONFIG_BYTES])
    }

    fn queue(&mut self, due_us: u64, tag: u16, payload: Vec<u8>) {
        self.outbox.push(Outgoing {
            due_us,
            tag,
            payload,
        });
    }
}

fn ap_error(ap: &ScriptedAp) -> super::ap::ApError {
    ap.check().expect_err("the caller found this one bad")
}

/// The capture every refusal of the association half names: the one successful association and
/// clean leave silicon has shown.
const ASSOC_CAPTURE: &str = "m12-wifi-assoc-success-2026-09-22";

/// Refuses, by name, an association path the device captures never exercised. `what` never
/// holds a key or password.
fn unexercised(handler: &str, what: &str) -> HleAction {
    HleAction::Fail(HleError::new(
        HleErrorKind::Handler,
        format!(
            "{handler}: {what}, which is unmodelled: the class A capture `{ASSOC_CAPTURE}` made one \
             WPA2-PSK association with the right key and one clean leave, and \
             `m12-wifi-assoc-2026-09-22` one connect at an SSID nothing answers; nothing else is \
             measured"
        ),
    ))
}

mod step {
    pub const ENTRY: u32 = 0;
    pub const SEMAPHORE: u32 = 1;
    pub const CALLER_SEMAPHORE: u32 = 2;
    pub const TASK: u32 = 3;
    pub const INTR: u32 = 4;
    pub const MAC: u32 = 5;
    pub const DELETE_TASK: u32 = 6;
    pub const DELETE_QUEUE: u32 = 7;
    pub const DELETE_CALLER_QUEUE: u32 = 8;
    pub const INTR_FREED: u32 = 9;
    pub const PARKED: u32 = 10;
    pub const WORKER_IDLE: u32 = 11;
    pub const WORKER_DID: u32 = 12;
    pub const TIMESTAMP: u32 = 13;
    pub const LOGGED: u32 = 14;
    pub const RX_MALLOC: u32 = 15;
    /// The registered RX callback is out, holding the buffer the worker just lent it.
    pub const RX_CALLBACK: u32 = 16;
    pub const RX_FREED: u32 = 17;
}

mod w {
    pub const STEP: usize = 0;
    pub const RET: usize = 1;
    pub const LINE: usize = 2;
    pub const LEN: usize = 4;
}

fn handler_name(kind: HandlerKind) -> Option<&'static str> {
    Some(match kind.0 {
        handler::INIT => "wifi.init",
        handler::DEINIT => "wifi.deinit",
        handler::SET_MODE => "wifi.set_mode",
        handler::GET_MODE => "wifi.get_mode",
        handler::SET_STORAGE => "wifi.set_storage",
        handler::SET_CONFIG => "wifi.set_config",
        handler::GET_CONFIG => "wifi.get_config",
        handler::GET_MAC => "wifi.get_mac",
        handler::START => "wifi.start",
        handler::STOP => "wifi.stop",
        handler::SCAN_START => "wifi.scan_start",
        handler::SCAN_STOP => "wifi.scan_stop",
        handler::SCAN_GET_AP_NUM => "wifi.scan_get_ap_num",
        handler::SCAN_GET_AP_RECORDS => "wifi.scan_get_ap_records",
        handler::REG_RXCB => "wifi.reg_rxcb",
        handler::REG_NETSTACK_BUF_CB => "wifi.reg_netstack_buf_cb",
        handler::FREE_RX_BUFFER => "wifi.free_rx_buffer",
        handler::CONNECT => "wifi.connect",
        handler::DISCONNECT => "wifi.disconnect",
        handler::TX => "wifi.tx",
        handler::SET_STA_IP => "wifi.set_sta_ip",
        _ if kind == worker_handler(MagicKind::WifiWorker) => "wifi.worker",
        _ => return None,
    })
}

pub struct WifiHost {
    module: ModuleIndex,
    profile: &'static WifiProfile,
    lines: &'static LogLines,
    pcs: MagicPcs,
    addrs: BTreeMap<String, u32>,
    synth: LogSynth,
    synth_bare: LogSynth,
    lines_verified: bool,
    wake: WakeMode,
}

impl WifiHost {
    /// `None` when the image does not link the driver or lacks a symbol the handlers need (binding
    /// already refused such an image).
    pub fn new(module: ModuleIndex, image: &ImageView<'_>) -> Option<WifiHost> {
        let profile = WifiProfile::load();
        let symbols = &image.elf.symbols;
        if profile
            .hooks
            .iter()
            .all(|hook| symbols.addr_of(&hook.name).is_none())
        {
            return None;
        }
        let addrs = hle_common::symbol_addrs(symbols, &profile.calls, &profile.data)?;
        let lines = LogLines::load();
        let log = |format: &str| {
            LogSynth::new(
                addrs.get("esp_log").copied(),
                addrs.get("esp_log_timestamp").copied(),
                format,
            )
        };
        let synth = log(&lines.info_format).ok()?;
        let synth_bare = log(&lines.info_bare_format).ok()?;
        let lines_verified = hle_common::init_lines_verified(image, profile.hooks.first()?);
        Some(WifiHost {
            module,
            profile,
            lines,
            synth,
            synth_bare,
            lines_verified,
            pcs: MagicPcs::from_spec()?,
            addrs,
            wake: WakeMode::default(),
        })
    }

    fn addr(&self, name: &str) -> u32 {
        self.addrs.get(name).copied().unwrap_or(0)
    }

    fn calls(&self) -> WorkerCalls {
        hle_common::worker_calls(&self.addrs)
    }

    fn set_inited(&self, g: &mut dyn GuestView, value: bool) -> Result<(), HleAction> {
        let at = self.addr("s_wifi_inited");
        g.write(at, &[u8::from(value)])
            .map_err(|_| hle_common::fault("wifi", "s_wifi_inited", at))
    }

    fn inited(&self, g: &mut dyn GuestView) -> Result<bool, HleAction> {
        let at = self.addr("s_wifi_inited");
        let mut byte = [0u8];
        g.read(at, &mut byte)
            .map(|()| byte[0] != 0)
            .map_err(|_| hle_common::fault("wifi", "s_wifi_inited", at))
    }

    /// Parks the calling task on the one caller semaphore, or refuses when another task is already
    /// parked. The handler continues at [`step::PARKED`].
    fn park_caller(
        &self,
        what: &'static str,
        st: &mut WifiState,
        words: &mut [u32],
        g: &mut dyn GuestView,
    ) -> HleAction {
        let task = g.current_task();
        if st.parked_caller != 0 && st.parked_caller != task {
            return HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                format!(
                    "{what}: task {task:#010x} called a blocking wifi API while task {:#010x} is \
                     parked on the module's one caller semaphore",
                    st.parked_caller
                ),
            ));
        }
        st.parked_caller = task;
        words[w::RET] = ESP_OK;
        words[w::STEP] = step::PARKED;
        HleAction::from(self.calls().park(st.caller_semaphore, PORT_MAX_DELAY))
    }

    fn unpark(&self, st: &mut WifiState, words: &[u32]) -> HleAction {
        st.parked_caller = 0;
        ret(words[w::RET])
    }

    /// `xQueueGenericSend(sem, NULL, 0, queueSEND_TO_BACK)`: the task-context give that releases a
    /// parked caller.
    fn give(&self, semaphore: u32) -> CallRequest {
        CallRequest::new(
            "xQueueGenericSend",
            self.addr("xQueueGenericSend"),
            &[
                Arg::Val(semaphore),
                Arg::Val(0),
                Arg::Val(0),
                Arg::Val(QUEUE_SEND_TO_BACK),
            ],
        )
    }

    fn arm(&self, g: &mut dyn GuestView, at: u64, tag: u16) {
        g.schedule(VTime::from_us(at), module_timer(self.module, tag));
    }

    fn timestamp_call(&self) -> HleAction {
        hle_common::timestamp_call(&self.synth, "wifi")
    }

    /// `esp_log(ESP_LOG_INFO, tag, format, timestamp, tag, text)`: the blob prints with no space
    /// after the tag, the app's `wifi_init` lines with one.
    fn log_call(&self, line: &crate::log_lines::LogLine, text: &str, timestamp: u32) -> HleAction {
        let template = LogLineTemplate::new(LogLevel::Info, &line.tag, text);
        let synth = if line.bare {
            &self.synth_bare
        } else {
            &self.synth
        };
        match synth.call(&template, timestamp) {
            Some(call) => HleAction::from(call),
            None => hle_common::missing_log_functions("wifi"),
        }
    }

    fn stage_lines(&self, stage: Stage) -> impl Iterator<Item = &crate::log_lines::LogLine> {
        self.lines.module_stage("wifi", stage, self.lines_verified)
    }

    fn lines_or(
        &self,
        stage: Stage,
        st: &mut WifiState,
        words: &mut [u32],
        g: &mut dyn GuestView,
    ) -> HleAction {
        if self
            .stage_lines(stage)
            .nth(words[w::LINE] as usize)
            .is_some()
        {
            words[w::STEP] = step::TIMESTAMP;
            return self.timestamp_call();
        }
        match stage {
            Stage::Init => {
                if let Err(fail) = self.set_inited(g, true) {
                    return fail;
                }
                ret(ESP_OK)
            }
            // The device posts event 43 and then `STA_START`, both about 39 ms after the call, and
            // only then does `esp_wifi_start` return.
            Stage::Start => {
                let due = g.now().as_us() + self.profile.driver.start_us();
                self.queue_at(st, g, due, self.profile.driver.event_start_first, &[]);
                self.queue_at(
                    st,
                    g,
                    due + POST_US,
                    self.profile.driver.event_sta_start,
                    &[],
                );
                st.queue(due + POST_US, work::WAKE, Vec::new());
                self.park_caller("wifi.start", st, words, g)
            }
            Stage::Stop => {
                self.queue_event(st, g, self.profile.driver.event_sta_stop, &[], true);
                self.park_caller("wifi.stop", st, words, g)
            }
            Stage::Deinit => self.finish_deinit(st, g),
            Stage::DeinitRefused => ret(words[w::RET]),
            Stage::Enable => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                "the wifi module has no enable stage",
            )),
        }
    }

    fn log_step(
        &self,
        stage: Stage,
        st: &mut WifiState,
        words: &mut [u32],
        g: &mut dyn GuestView,
        a0: u32,
    ) -> HleAction {
        if words[w::STEP] == step::TIMESTAMP {
            let Some(line) = self.stage_lines(stage).nth(words[w::LINE] as usize) else {
                return bad_step("wifi log line", words[w::LINE]);
            };
            let mac = hle_common::mac_text(&st.sta_mac);
            let text = line
                .text
                .replace("{mac}", &mac)
                .replace("{task}", &format!("{:x}", st.worker));
            words[w::STEP] = step::LOGGED;
            return self.log_call(line, &text, a0);
        }
        st.synthesized_lines += 1;
        words[w::LINE] += 1;
        self.lines_or(stage, st, words, g)
    }

    fn queue_event(
        &self,
        st: &mut WifiState,
        g: &mut dyn GuestView,
        id: u32,
        data: &[u8],
        wake_caller: bool,
    ) {
        let due = g.now().as_us() + POST_US;
        self.queue_at(st, g, due, id, data);
        if wake_caller {
            st.queue(due, work::WAKE, Vec::new());
        }
    }

    fn queue_at(&self, st: &mut WifiState, g: &mut dyn GuestView, due: u64, id: u32, data: &[u8]) {
        let mut payload = id.to_le_bytes().to_vec();
        payload.extend_from_slice(data);
        st.queue(due, work::POST, payload);
        self.arm(g, due, timer::DISPATCH);
    }

    /// `wifi_event_sta_scan_done_t` is `{status, number, scan_id}`. `status` is 1 for a sweep cut
    /// short, which the device posts for both an aborted and a stopped scan.
    fn scan_done(&self, st: &mut WifiState, g: &mut dyn GuestView, status: u8, number: usize) {
        let mut data = vec![0u8; self.profile.driver.scan_done_bytes as usize];
        data[0] = status;
        data[4] = u8::try_from(number).unwrap_or(u8::MAX);
        data[5] = st.scan_id as u8;
        let due = g.now().as_us() + POST_US;
        self.queue_at(st, g, due, self.profile.driver.event_scan_done, &data);
    }

    /// The whole plan for an image of the shape whose sdkconfig was read, only the rows without a
    /// `config` for any other. The RX row reads its capacity from the guest, so every image gets
    /// it.
    pub fn heap_plan(&self) -> Vec<&'static LedgerRow> {
        self.profile.heap_plan(self.lines_verified)
    }

    /// How many buffers may be out is [`WifiState::rx_capacity`], the guest's own value.
    fn rx_row(&self) -> Option<&'static LedgerRow> {
        self.heap_plan().into_iter().find(|r| r.label == RX_ROW)
    }

    fn rx_row_index(&self) -> Option<u16> {
        let at = self.heap_plan().iter().position(|r| r.label == RX_ROW)?;
        u16::try_from(at).ok()
    }

    /// Offers one Ethernet frame to interface `ifx` of the guest's netstack.
    ///
    /// The frame takes the same [`POST_US`] hop to the worker as every other delivery: a callback run
    /// inside the producing call would run on the wrong task at the wrong instant. Whether a buffer
    /// can be lent is decided when the worker reaches it, since capacity is a fact of that instant.
    pub fn receive_frame(&self, st: &mut WifiState, g: &mut dyn GuestView, ifx: u8, frame: &[u8]) {
        let due = g.now().as_us() + POST_US;
        let mut payload = Vec::with_capacity(frame.len() + 1);
        payload.push(ifx);
        payload.extend_from_slice(frame);
        st.queue(due, work::RX, payload);
        self.arm(g, due, timer::DISPATCH);
    }

    fn partial(&self, st: &WifiState, elapsed_us: u64) -> Vec<ScriptedAp> {
        let swept = self.profile.driver.swept_channels(elapsed_us);
        let heard: Vec<ScriptedAp> = st
            .aps
            .iter()
            .filter(|ap| u32::from(ap.channel) <= swept)
            .cloned()
            .collect();
        super::ap::heard(&heard)
    }
}

impl WifiHost {
    fn init(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let (a0, scratch) = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.inited(g) {
                    Ok(true) => return ret(ESP_OK),
                    Ok(false) => {}
                    Err(fail) => return fail,
                }
                let cfg = g.reg(A0);
                if cfg == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                let at = cfg + self.profile.driver.init_config_magic_off;
                let magic = match pemu_hle::guest_call::read_u32(g, at) {
                    Ok(magic) => magic,
                    Err(_) => return hle_common::fault("wifi", "wifi_init_config_t", at),
                };
                if magic != self.profile.driver.init_config_magic {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                // The guest's own `DYNAMIC_RX_BUFFER_NUM`, which `WIFI_INIT_CONFIG_DEFAULT()` put
                // in the struct.
                let at = cfg + self.profile.driver.init_config_dynamic_rx_buf_num_off;
                st.rx_capacity = match pemu_hle::guest_call::read_u32(g, at) {
                    Ok(count) => count,
                    Err(_) => return hle_common::fault("wifi", "wifi_init_config_t", at),
                };
                words[w::STEP] = step::SEMAPHORE;
                HleAction::from(self.calls().create_semaphore())
            }
            step::SEMAPHORE => {
                if a0 == 0 {
                    return ret(ESP_ERR_NO_MEM);
                }
                st.semaphore = a0;
                words[w::STEP] = step::CALLER_SEMAPHORE;
                HleAction::from(self.calls().create_semaphore())
            }
            step::CALLER_SEMAPHORE => {
                if a0 == 0 {
                    return ret(ESP_ERR_NO_MEM);
                }
                st.caller_semaphore = a0;
                let mut profile = wifi_profile();
                profile.stack_bytes = self.profile.worker.min_stack;
                profile.priority = self.profile.worker.priority;
                words[w::STEP] = step::TASK;
                HleAction::from(
                    self.calls()
                        .create_worker(&self.pcs, &profile, st.semaphore),
                )
            }
            step::TASK => {
                // pdPASS is 1; the handle is the `&handle` out-parameter at scratch offset 0.
                if a0 != 1 {
                    return ret(ESP_ERR_NO_MEM);
                }
                st.worker = word_at(scratch, 0);
                if self.wake == WakeMode::U4MagicIsr {
                    words[w::STEP] = step::INTR;
                    return HleAction::from(
                        CallRequest::new(
                            "esp_intr_alloc",
                            self.addr("esp_intr_alloc"),
                            &[
                                Arg::Val(u32::from(self.profile.worker.isr_source)),
                                Arg::Val(INTR_FLAG_LEVEL1),
                                Arg::Val(self.pcs.pc_of(MagicKind::WifiIsr)),
                                Arg::Val(0),
                                Arg::Scratch(0),
                            ],
                        )
                        .with_scratch(vec![0; 4]),
                    );
                }
                self.read_mac(words)
            }
            step::INTR => {
                if a0 != ESP_OK {
                    return ret(a0);
                }
                st.intr_handle = word_at(scratch, 0);
                self.read_mac(words)
            }
            step::MAC => {
                if let Some(mac) = scratch.get(..6) {
                    st.sta_mac.copy_from_slice(mac);
                }
                st.state = DriverState::Init;
                st.mode = 0;
                st.storage = 0;
                st.records.clear();
                st.outbox.clear();
                st.pending.clear();
                words[w::LINE] = 0;
                self.lines_or(Stage::Init, st, words, g)
            }
            step::TIMESTAMP | step::LOGGED => self.log_step(Stage::Init, st, words, g, a0),
            other => bad_step("wifi.init", other),
        }
    }

    /// `esp_read_mac(&mac, ESP_MAC_WIFI_STA)`, so the eFuse base-MAC logic stays guest code.
    fn read_mac(&self, words: &mut [u32]) -> HleAction {
        words[w::STEP] = step::MAC;
        HleAction::from(
            CallRequest::new(
                "esp_read_mac",
                self.addr("esp_read_mac"),
                &[Arg::Scratch(0), Arg::Val(ESP_MAC_WIFI_STA)],
            )
            .with_scratch(vec![0; 8]),
        )
    }

    fn deinit(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let _ = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.inited(g) {
                    Ok(false) => return ret(ESP_ERR_WIFI_NOT_INIT),
                    Ok(true) => {}
                    Err(fail) => return fail,
                }
                if st.state.started() {
                    words[w::RET] = ESP_ERR_WIFI_NOT_STOPPED;
                    words[w::LINE] = 0;
                    return self.lines_or(Stage::DeinitRefused, st, words, g);
                }
                words[w::STEP] = step::DELETE_TASK;
                HleAction::from(CallRequest::new(
                    "vTaskDelete",
                    self.addr("vTaskDelete"),
                    &[Arg::Val(st.worker)],
                ))
            }
            step::DELETE_TASK => {
                words[w::STEP] = step::DELETE_QUEUE;
                HleAction::from(CallRequest::new(
                    "vQueueDelete",
                    self.addr("vQueueDelete"),
                    &[Arg::Val(st.semaphore)],
                ))
            }
            step::DELETE_QUEUE => {
                words[w::STEP] = step::DELETE_CALLER_QUEUE;
                HleAction::from(CallRequest::new(
                    "vQueueDelete",
                    self.addr("vQueueDelete"),
                    &[Arg::Val(st.caller_semaphore)],
                ))
            }
            step::DELETE_CALLER_QUEUE => {
                if st.intr_handle != 0 {
                    words[w::STEP] = step::INTR_FREED;
                    let handle = st.intr_handle;
                    st.intr_handle = 0;
                    return HleAction::from(CallRequest::new(
                        "esp_intr_free",
                        self.addr("esp_intr_free"),
                        &[Arg::Val(handle)],
                    ));
                }
                words[w::LINE] = 0;
                self.lines_or(Stage::Deinit, st, words, g)
            }
            step::INTR_FREED => {
                words[w::LINE] = 0;
                self.lines_or(Stage::Deinit, st, words, g)
            }
            step::TIMESTAMP | step::LOGGED => {
                let (a0, _) = returned(resume);
                // Only the refusal sets a return code before its line.
                let stage = if words[w::RET] == ESP_ERR_WIFI_NOT_STOPPED {
                    Stage::DeinitRefused
                } else {
                    Stage::Deinit
                };
                self.log_step(stage, st, words, g, a0)
            }
            other => bad_step("wifi.deinit", other),
        }
    }

    fn finish_deinit(&self, st: &mut WifiState, g: &mut dyn GuestView) -> HleAction {
        let aps = std::mem::take(&mut st.aps);
        let lan = std::mem::take(&mut st.lan);
        let capture = std::mem::take(&mut st.capture);
        *st = WifiState {
            // The scripted world is the machine's, not the driver's: a deinit does not unscript the
            // air, nor take down the LAN behind it or its capture.
            aps,
            lan,
            capture,
            ..WifiState::default()
        };
        if let Err(fail) = self.set_inited(g, false) {
            return fail;
        }
        ret(ESP_OK)
    }

    fn start_stop(
        &self,
        start: bool,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let _ = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.inited(g) {
                    Ok(false) => return ret(ESP_ERR_WIFI_NOT_INIT),
                    Ok(true) => {}
                    Err(fail) => return fail,
                }
                if start == st.state.started() {
                    return ret(ESP_OK);
                }
                if start {
                    st.state = DriverState::Started;
                    words[w::LINE] = 0;
                    return self.lines_or(Stage::Start, st, words, g);
                }
                if st.state == DriverState::Connected {
                    // Class A: a station stopped while associated leaves with one
                    // `STA_DISCONNECTED` of reason 8 (`WIFI_REASON_ASSOC_LEAVE`), then `STA_STOP`,
                    // both before `esp_wifi_stop` returns.
                    let peer = st.peer.take();
                    let driver = &self.profile.driver;
                    let data =
                        self.disconnected_payload(st, peer.as_ref(), driver.reason_assoc_leave);
                    self.queue_event(st, g, driver.event_sta_disconnected, &data, false);
                }
                // A stop cancels a sweep in flight, and the device posts that sweep's own
                // `SCAN_DONE` (status 1, the partial count) before `STA_STOP`. That event releases
                // a parked blocking caller like any other abort, so the stop's own wake is the one
                // that returns the task.
                if st.state == DriverState::Scanning {
                    let elapsed = g.now().as_us().saturating_sub(st.scan_started_us);
                    st.records = self.partial(st, elapsed);
                    let number = st.records.len();
                    self.scan_done(st, g, 1, number);
                }
                st.state = DriverState::Init;
                st.scan_blocking = false;
                st.scan_due_us = 0;
                // A stop also ends an outstanding `esp_wifi_connect` attempt, with no disconnect
                // event: the attempt never reached one.
                st.connect_due_us = 0;
                st.peer = None;
                // A stopped driver answers 12290 to every scan query, so the partial list is not
                // readable afterwards.
                st.records.clear();
                words[w::LINE] = 0;
                self.lines_or(Stage::Stop, st, words, g)
            }
            step::TIMESTAMP | step::LOGGED => {
                let (a0, _) = returned(resume);
                let stage = if start { Stage::Start } else { Stage::Stop };
                self.log_step(stage, st, words, g, a0)
            }
            step::PARKED => self.unpark(st, words),
            other => bad_step(if start { "wifi.start" } else { "wifi.stop" }, other),
        }
    }

    fn scan_start(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let _ = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.inited(g) {
                    Ok(false) => return ret(ESP_ERR_WIFI_NOT_INIT),
                    Ok(true) => {}
                    Err(fail) => return fail,
                }
                if !st.state.started() {
                    return ret(ESP_ERR_WIFI_NOT_STARTED);
                }
                // A sweep shares the one radio with an association, and no capture scanned while
                // associated or associating. An attempt at an SSID nothing answers is itself a
                // sweep and does not block.
                if st.peer.is_some() {
                    return unexercised(
                        "wifi.scan_start",
                        "a scan while the station is associated or associating",
                    );
                }
                let block = g.reg(A0 + 1) != 0;
                let now = g.now().as_us();
                if st.state == DriverState::Scanning {
                    // The device does not refuse a second scan: it cancels the running one, posts
                    // its `SCAN_DONE` with status 1 and no access point, keeps the `scan_id` and
                    // starts the new sweep.
                    st.records.clear();
                    self.scan_done(st, g, 1, 0);
                    if st.scan_blocking {
                        // The cancelled scan's caller is released by that `SCAN_DONE` and answers
                        // ESP_OK; its next `esp_wifi_scan_get_ap_num` reads the 0 records the abort
                        // left.
                        st.scan_blocking = false;
                        st.queue(now + POST_US, work::WAKE, Vec::new());
                        self.arm(g, now + POST_US, timer::DISPATCH);
                    }
                } else {
                    // A fresh sweep moves `scan_id`; a cancelled sweep and its replacement share
                    // one. Starting at 1 rather than the blob's own high value is class C: that
                    // origin is internal to the blob.
                    st.scan_id = st.scan_id.wrapping_add(1);
                    st.records.clear();
                }
                st.state = DriverState::Scanning;
                st.scan_started_us = now;
                st.scan_due_us = now + self.profile.driver.scan_us();
                st.scan_blocking = block;
                self.arm(g, st.scan_due_us, timer::SCAN);
                if !block {
                    return ret(ESP_OK);
                }
                self.park_caller("wifi.scan_start", st, words, g)
            }
            step::PARKED => self.unpark(st, words),
            other => bad_step("wifi.scan_start", other),
        }
    }

    fn immediate(&self, kind: u16, st: &mut WifiState, g: &mut dyn GuestView) -> HleAction {
        let inited = match self.inited(g) {
            Ok(inited) => inited,
            Err(fail) => return fail,
        };
        if !inited {
            return ret(ESP_ERR_WIFI_NOT_INIT);
        }
        let (a0, a1) = (g.reg(A0), g.reg(A0 + 1));
        match kind {
            handler::SET_MODE => {
                if a0 > MAX_MODE {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                st.mode = a0;
                ret(ESP_OK)
            }
            handler::GET_MODE => {
                if a0 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                match g.write(a0, &st.mode.to_le_bytes()) {
                    Ok(()) => ret(ESP_OK),
                    Err(_) => hle_common::fault("wifi", "wifi_mode_t", a0),
                }
            }
            handler::SET_STORAGE => {
                if a0 > MAX_STORAGE {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                st.storage = a0;
                ret(ESP_OK)
            }
            handler::SET_CONFIG => {
                if a0 > MAX_IF {
                    return ret(ESP_ERR_WIFI_IF);
                }
                if a1 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                let mut bytes = vec![0u8; CONFIG_BYTES];
                if g.read(a1, &mut bytes).is_err() {
                    return hle_common::fault("wifi", "wifi_config_t", a1);
                }
                let ifx = a0 as usize;
                if st.config.len() <= ifx {
                    st.config.resize(ifx + 1, vec![0u8; CONFIG_BYTES]);
                }
                st.config[ifx] = bytes;
                ret(ESP_OK)
            }
            handler::GET_CONFIG => {
                if a0 > MAX_IF {
                    return ret(ESP_ERR_WIFI_IF);
                }
                if a1 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                match g.write(a1, &st.config_of(a0 as usize)) {
                    Ok(()) => ret(ESP_OK),
                    Err(_) => hle_common::fault("wifi", "wifi_config_t", a1),
                }
            }
            handler::GET_MAC => {
                if a0 > MAX_IF {
                    return ret(ESP_ERR_WIFI_IF);
                }
                if a1 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                match g.write(a1, &st.sta_mac) {
                    Ok(()) => ret(ESP_OK),
                    Err(_) => hle_common::fault("wifi", "the station MAC", a1),
                }
            }
            handler::SCAN_STOP => {
                if !st.state.started() {
                    return ret(ESP_ERR_WIFI_NOT_STARTED);
                }
                if st.state == DriverState::Scanning {
                    // The device answers ESP_OK and posts `SCAN_DONE` with status 1 and the access
                    // points the sweep had already heard.
                    let elapsed = g.now().as_us().saturating_sub(st.scan_started_us);
                    st.records = self.partial(st, elapsed);
                    let number = st.records.len();
                    self.scan_done(st, g, 1, number);
                    st.state = DriverState::Started;
                    st.scan_due_us = 0;
                    if st.scan_blocking {
                        // The stopped scan's caller is released by that `SCAN_DONE`, and the
                        // partial list stays readable.
                        st.scan_blocking = false;
                        let due = g.now().as_us() + POST_US;
                        st.queue(due, work::WAKE, Vec::new());
                        self.arm(g, due, timer::DISPATCH);
                    }
                }
                ret(ESP_OK)
            }
            handler::SCAN_GET_AP_NUM => {
                // A stopped driver answers 12290 to a scan query, as it does to
                // `esp_wifi_scan_start`.
                if !st.state.started() {
                    return ret(ESP_ERR_WIFI_NOT_STARTED);
                }
                if a0 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                let number = u16::try_from(st.records.len()).unwrap_or(u16::MAX);
                match g.write(a0, &number.to_le_bytes()) {
                    Ok(()) => ret(ESP_OK),
                    Err(_) => hle_common::fault("wifi", "the record count", a0),
                }
            }
            handler::SCAN_GET_AP_RECORDS => {
                if !st.state.started() {
                    return ret(ESP_ERR_WIFI_NOT_STARTED);
                }
                if a0 == 0 || a1 == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                let mut want = [0u8; 2];
                if g.read(a0, &mut want).is_err() {
                    return hle_common::fault("wifi", "the record count", a0);
                }
                let want = usize::from(u16::from_le_bytes(want));
                let size = self.profile.driver.ap_record_bytes as usize;
                let count = want.min(st.records.len());
                let mut bytes = vec![0u8; count * size];
                for (i, ap) in st.records.iter().take(count).enumerate() {
                    super::ap::write_record(ap, &mut bytes[i * size..(i + 1) * size]);
                }
                if !bytes.is_empty() && g.write(a1, &bytes).is_err() {
                    return hle_common::fault("wifi", "the record list", a1);
                }
                let written = u16::try_from(count).unwrap_or(u16::MAX);
                if g.write(a0, &written.to_le_bytes()).is_err() {
                    return hle_common::fault("wifi", "the record count", a0);
                }
                st.records.clear();
                ret(ESP_OK)
            }
            handler::REG_RXCB => {
                if a0 > MAX_IF {
                    return ret(ESP_ERR_WIFI_IF);
                }
                let ifx = a0 as usize;
                if st.rxcb.len() <= ifx {
                    st.rxcb.resize(ifx + 1, 0);
                }
                st.rxcb[ifx] = a1;
                ret(ESP_OK)
            }
            handler::REG_NETSTACK_BUF_CB => {
                st.netstack_cb = [a0, a1];
                ret(ESP_OK)
            }
            other => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                format!("the wifi module has no immediate handler {other}"),
            )),
        }
    }

    /// Read at the first post because the cache MMU does not map flash when the image loads.
    fn event_base(&self, st: &mut WifiState, g: &mut dyn GuestView) -> Result<u32, HleAction> {
        if st.event_base != 0 {
            return Ok(st.event_base);
        }
        let at = self.addr("WIFI_EVENT");
        let base = pemu_hle::guest_call::read_u32(g, at)
            .map_err(|_| hle_common::fault("wifi", "WIFI_EVENT", at))?;
        if base == 0 {
            return Err(HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                "WIFI_EVENT still reads 0: the event base is not mapped yet",
            )));
        }
        st.event_base = base;
        Ok(base)
    }

    fn worker(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        if let Resume::Woken {
            reason: WakeReason::Deinit,
        } = resume
        {
            words[w::STEP] = step::PARKED;
            return HleAction::Park;
        }
        // A received frame is the one item that takes more than one nested call, so a delivery
        // already in flight takes its next step before any new work is looked at.
        if words[w::STEP] == step::RX_MALLOC
            && let Some(next) = self.rx_malloced(st, words, g, resume)
        {
            return next;
        }
        if words[w::STEP] == step::RX_CALLBACK {
            // The buffer stays the driver's until `esp_wifi_internal_free_rx_buffer` gives it back,
            // whatever the callback answered.
            st.rx_delivered += 1;
        }
        loop {
            let Some(item) = st.pending.first().cloned() else {
                words[w::STEP] = step::WORKER_IDLE;
                return HleAction::Park;
            };
            if item.tag == work::RX {
                // The item stays at the head of `pending` until its buffer is in the ledger, so a
                // snapshot taken with the `heap_caps_malloc` outstanding restores with the frame
                // still to deliver.
                match self.rx_start(st, words) {
                    Some(call) => return call,
                    None => {
                        st.pending.remove(0);
                        continue;
                    }
                }
            }
            st.pending.remove(0);
            words[w::STEP] = step::WORKER_DID;
            return self.do_work(st, item, g);
        }
    }

    fn do_work(&self, st: &mut WifiState, item: Outgoing, g: &mut dyn GuestView) -> HleAction {
        match item.tag {
            work::POST => {
                let base = match self.event_base(st, g) {
                    Ok(base) => base,
                    Err(fail) => return fail,
                };
                let id = word_at(&item.payload, 0);
                let data = &item.payload[4.min(item.payload.len())..];
                let len = data.len() as u32;
                st.posted_events += 1;
                let mut scratch = data.to_vec();
                scratch.resize(
                    scratch
                        .len()
                        .max(pemu_hle::guest_call::SCRATCH_WORD)
                        .next_multiple_of(4),
                    0,
                );
                HleAction::from(
                    CallRequest::new(
                        "esp_event_post",
                        self.addr("esp_event_post"),
                        &[
                            Arg::Val(base),
                            Arg::Val(id),
                            if len == 0 {
                                Arg::Val(0)
                            } else {
                                Arg::Scratch(0)
                            },
                            Arg::Val(len),
                            Arg::Val(0),
                        ],
                    )
                    .with_scratch(scratch),
                )
            }
            work::WAKE => HleAction::from(self.give(st.caller_semaphore)),
            other => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                format!("the wifi worker was given unknown work {other}"),
            )),
        }
    }

    /// Takes the RX buffer out of the image's own allocator, as the real driver does. `None` is a
    /// frame dropped before any buffer was asked for: no RX callback, the frame does not fit one
    /// buffer, or all `DYNAMIC_RX_BUFFER_NUM` buffers are still out.
    fn rx_start(&self, st: &mut WifiState, words: &mut [u32]) -> Option<HleAction> {
        let (ifx, len) = match st.pending.first().and_then(|i| i.payload.split_first()) {
            Some((ifx, frame)) => (usize::from(*ifx), frame.len()),
            None => return None,
        };
        // An image whose sdkconfig this build has not read lends nothing, which shows as dropped
        // frames rather than silence.
        if st.rx_capacity == 0 && st.state.inited() {
            return Some(HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                "wifi RX: the guest's wifi_init_config_t asks for dynamic_rx_buf_num 0, which is \
                 unmodelled: no capture shows how many RX buffers the driver lends then",
            )));
        }
        let lend = self.rx_row().is_some_and(|row| {
            len > 0
                && len <= row.element as usize
                && st.rxcb.get(ifx).copied().unwrap_or(0) != 0
                && st.ledger.blocks.len() < st.rx_capacity as usize
        });
        if !lend {
            st.rx_dropped += 1;
            return None;
        }
        words[w::STEP] = step::RX_MALLOC;
        Some(HleAction::from(CallRequest::new(
            "heap_caps_malloc",
            self.addr("heap_caps_malloc"),
            &[Arg::Val(len as u32), Arg::Val(LEDGER_CAPS)],
        )))
    }

    /// The allocator answered. `None` means the frame was dropped; `Some` is the nested call into
    /// the registered RX callback.
    fn rx_malloced(
        &self,
        st: &mut WifiState,
        words: &mut [u32],
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> Option<HleAction> {
        let (addr, _) = returned(resume);
        let item = st.pending.first().cloned()?;
        let (ifx, frame) = item.payload.split_first()?;
        let (ifx, len) = (usize::from(*ifx), frame.len());
        st.pending.remove(0);
        if addr == 0 {
            // A guest heap too tight for another RX buffer loses the frame, as a device whose
            // `heap_caps_malloc` fails does.
            st.ledger.refused += 1;
            st.rx_dropped += 1;
            words[w::STEP] = step::WORKER_DID;
            return None;
        }
        let Some(index) = self.rx_row_index() else {
            st.rx_dropped += 1;
            words[w::STEP] = step::WORKER_DID;
            return None;
        };
        st.ledger.record(index, addr, len as u32);
        if g.write(addr, frame).is_err() {
            return Some(hle_common::fault("wifi", "an RX buffer", addr));
        }
        let cb = st.rxcb.get(ifx).copied().unwrap_or(0);
        if cb == 0 {
            st.rx_dropped += 1;
            words[w::STEP] = step::WORKER_DID;
            return None;
        }
        st.capture.record(g.now().as_us(), pcap::dir::RX, frame);
        words[w::STEP] = step::RX_CALLBACK;
        // `wifi_rxcb_t` is `esp_err_t (*)(void *buffer, uint16_t len, void *eb)`. The buffer is its
        // own `eb` handle: one block is lent, and one block comes back.
        Some(HleAction::from(CallRequest::new(
            "wifi_rxcb",
            cb,
            &[Arg::Val(addr), Arg::Val(len as u32), Arg::Val(addr)],
        )))
    }

    /// `esp_wifi_internal_free_rx_buffer(void *eb)`: the buffer goes back to the image's allocator
    /// and leaves the ledger, freeing capacity for the next frame. The real function returns
    /// `void`. An address this module never lent is counted and not freed.
    fn free_rx_buffer(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let _ = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                let eb = g.reg(A0);
                let Some(at) = st.ledger.blocks.iter().position(|b| b.addr == eb) else {
                    st.rx_alien_free += 1;
                    return ret(ESP_OK);
                };
                st.ledger.blocks.remove(at);
                st.rx_freed += 1;
                words[w::STEP] = step::RX_FREED;
                HleAction::from(CallRequest::new(
                    "heap_caps_free",
                    self.addr("heap_caps_free"),
                    &[Arg::Val(eb)],
                ))
            }
            step::RX_FREED => ret(ESP_OK),
            other => bad_step("wifi.free_rx_buffer", other),
        }
    }

    /// `esp_wifi_connect_internal`. It returns 0 at once; the outcome, decided here from the world
    /// at the call, arrives later as events:
    ///
    /// - no scripted access point serves the configured SSID: one channel sweep, then two
    ///   disconnects;
    /// - one does, in the measured configuration: the station associates
    ///   ([`WifiHost::finish_connect`]);
    /// - one does in any other configuration: refused by name ([`WifiHost::admits`]), since nothing
    ///   says what the driver does then.
    fn connect(&self, st: &mut WifiState, g: &mut dyn GuestView) -> HleAction {
        match self.inited(g) {
            Ok(false) => return ret(ESP_ERR_WIFI_NOT_INIT),
            Ok(true) => {}
            Err(fail) => return fail,
        }
        if !st.state.started() {
            return ret(ESP_ERR_WIFI_NOT_STARTED);
        }
        if st.state == DriverState::Connected {
            return unexercised(
                "wifi.connect",
                "esp_wifi_connect on a station that is already associated",
            );
        }
        let peer = match self.answering_ap(st) {
            None => None,
            Some(ap) => {
                if let Err(what) = self.admits(st, &ap) {
                    return unexercised("wifi.connect", &what);
                }
                if st.state == DriverState::Scanning {
                    return unexercised(
                        "wifi.connect",
                        "a connect at an SSID a scripted access point serves while a scan runs",
                    );
                }
                super::ap::heard(std::slice::from_ref(&ap)).pop()
            }
        };
        let now = g.now().as_us();
        let driver = &self.profile.driver;
        // A second `esp_wifi_connect` while one is outstanding was never captured. Restarting the
        // attempt needs no new fact: the one outstanding attempt is the latest.
        st.connect_due_us = now
            + if peer.is_some() {
                driver.associate_us()
            } else {
                driver.connect_us()
            };
        st.peer = peer;
        if st.state == DriverState::Started {
            st.state = DriverState::Connecting;
        }
        self.arm(g, st.connect_due_us, timer::CONNECT);
        ret(ESP_OK)
    }

    /// Whether a connect at `ap` is the measured association (WPA2-PSK, right key, no BSSID or
    /// channel pinned), or the reason it is not. `WIFI_AUTH_OPEN` is the one extension, with the
    /// WPA2-PSK timing as class C. The password is compared, never copied into the answer.
    fn admits(&self, st: &WifiState, ap: &ScriptedAp) -> Result<(), String> {
        const WPA2_PSK: u8 = 3;
        let driver = &self.profile.driver;
        let config = st.config_of(0);
        let at = |off: u32| config.get(off as usize).copied().unwrap_or(0);
        if ap.authmode != super::ap::AUTH_OPEN && ap.authmode != WPA2_PSK {
            return Err(format!(
                "the scripted access point's auth mode is {} and only WIFI_AUTH_OPEN (0) and \
                 WIFI_AUTH_WPA2_PSK (3) associate",
                ap.authmode
            ));
        }
        let threshold = word_at(&config, driver.sta_config_threshold_authmode_off as usize);
        if u32::from(ap.authmode) < threshold {
            return Err(format!(
                "the station's threshold.authmode is {threshold}, which the scripted access \
                 point's auth mode {} cannot meet",
                ap.authmode
            ));
        }
        let off = driver.sta_config_password_off as usize;
        let field = config.get(off..off + 64).unwrap_or(&[]);
        let password = &field[..field.iter().position(|b| *b == 0).unwrap_or(field.len())];
        if ap.authmode == WPA2_PSK && password != ap.psk.as_slice() {
            return Err(
                "the configured password is not the scripted access point's key".to_string(),
            );
        }
        if ap.authmode == super::ap::AUTH_OPEN && !password.is_empty() {
            return Err("a password is configured for an open access point".to_string());
        }
        if at(driver.sta_config_bssid_set_off) != 0 {
            return Err("the station pins a BSSID (bssid_set)".to_string());
        }
        if at(driver.sta_config_channel_off) != 0 {
            return Err(format!(
                "the station pins channel {}",
                at(driver.sta_config_channel_off)
            ));
        }
        Ok(())
    }

    /// `esp_wifi_disconnect_internal`.
    ///
    /// Associated: exactly one `STA_DISCONNECTED` with reason 8 (`WIFI_REASON_ASSOC_LEAVE`), queued
    /// before the caller's release so the event loop delivers it first. Not associated: answers 0
    /// with no event, and an outstanding attempt is cancelled silently.
    fn disconnect(
        &self,
        st: &mut WifiState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let _ = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.inited(g) {
                    Ok(false) => return ret(ESP_ERR_WIFI_NOT_INIT),
                    Ok(true) => {}
                    Err(fail) => return fail,
                }
                if !st.state.started() {
                    return ret(ESP_ERR_WIFI_NOT_STARTED);
                }
                if st.state == DriverState::Connected {
                    let peer = st.peer.take();
                    st.state = DriverState::Started;
                    let driver = &self.profile.driver;
                    let data =
                        self.disconnected_payload(st, peer.as_ref(), driver.reason_assoc_leave);
                    self.queue_event(st, g, driver.event_sta_disconnected, &data, true);
                    return self.park_caller("wifi.disconnect", st, words, g);
                }
                st.connect_due_us = 0;
                st.peer = None;
                if st.state == DriverState::Connecting {
                    st.state = DriverState::Started;
                }
                ret(ESP_OK)
            }
            step::PARKED => self.unpark(st, words),
            other => bad_step("wifi.disconnect", other),
        }
    }

    /// `ssid` and `ssid_len` are the guest's own configured SSID. `bssid` and `rssi` are the peer's
    /// when associated, else zero. The probes read only `reason`, so the other fields are
    /// unverified shape.
    fn disconnected_payload(
        &self,
        st: &WifiState,
        peer: Option<&ScriptedAp>,
        reason: u32,
    ) -> Vec<u8> {
        let driver = &self.profile.driver;
        let mut data = vec![0u8; driver.disconnected_bytes as usize];
        let config = st.config_of(0);
        // `wifi_sta_config_t` opens with `uint8_t ssid[32]`; the `password` after it is a secret
        // and is deliberately not read.
        let ssid = &config[..32.min(config.len())];
        let len = ssid.iter().position(|b| *b == 0).unwrap_or(ssid.len());
        data[..len].copy_from_slice(&ssid[..len]);
        let mut put = |off: u32, bytes: &[u8]| {
            if let Some(slot) = data.get_mut(off as usize..off as usize + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
        };
        put(
            driver.disconnected_ssid_len_off,
            &[u8::try_from(len).unwrap_or(u8::MAX)],
        );
        put(driver.disconnected_reason_off, &[reason as u8]);
        if let Some(ap) = peer {
            put(driver.disconnected_bssid_off, &ap.bssid);
            put(driver.disconnected_rssi_off, &[ap.rssi as u8]);
        }
        data
    }

    /// The layout is the corpus DWARF's; values are the access point's own except `aid`, the
    /// profile's class C `connected_aid`. None of the payload was measured.
    fn connected_payload(&self, ap: &ScriptedAp) -> Vec<u8> {
        let driver = &self.profile.driver;
        let mut data = vec![0u8; driver.connected_bytes as usize];
        let ssid = ap.ssid.as_bytes();
        let len = ssid.len().min(super::ap::MAX_SSID);
        let mut put = |off: u32, bytes: &[u8]| {
            if let Some(slot) = data.get_mut(off as usize..off as usize + bytes.len()) {
                slot.copy_from_slice(bytes);
            }
        };
        put(0, &ssid[..len]);
        put(driver.connected_ssid_len_off, &[len as u8]);
        put(driver.connected_bssid_off, &ap.bssid);
        put(driver.connected_channel_off, &[ap.channel]);
        put(
            driver.connected_authmode_off,
            &u32::from(ap.authmode).to_le_bytes(),
        );
        put(
            driver.connected_aid_off,
            &u16::try_from(driver.connected_aid)
                .unwrap_or(u16::MAX)
                .to_le_bytes(),
        );
        data
    }

    /// Resolves an outstanding `esp_wifi_connect` whose instant came.
    ///
    /// At a scripted access point: `Connected`, with `STA_CONNECTED` queued `connected_us` after the
    /// call. At an SSID nothing answers: two `STA_DISCONNECTED`, reason 201 then 36, queued at one
    /// instant in that order, as the device posts both before the caller runs again.
    fn finish_connect(&self, st: &mut WifiState, g: &mut dyn GuestView, now_us: u64) {
        if st.connect_due_us == 0 || now_us < st.connect_due_us {
            return;
        }
        st.connect_due_us = 0;
        let driver = &self.profile.driver;
        let due = now_us + POST_US;
        if let Some(peer) = st.peer.clone() {
            // `connect` refuses while scanning and `scan_start` while connecting, so the attempt
            // resolves from `Connecting`.
            st.state = DriverState::Connected;
            let data = self.connected_payload(&peer);
            self.queue_at(st, g, due, driver.event_sta_connected, &data);
            return;
        }
        if st.state == DriverState::Connecting {
            st.state = DriverState::Started;
        }
        for reason in [driver.reason_no_ap_found, driver.reason_after_no_ap_found] {
            let data = self.disconnected_payload(st, None, reason);
            self.queue_at(st, g, due, driver.event_sta_disconnected, &data);
        }
    }

    fn answering_ap(&self, st: &WifiState) -> Option<ScriptedAp> {
        let config = st.config_of(0);
        let ssid = &config[..32.min(config.len())];
        let len = ssid.iter().position(|b| *b == 0).unwrap_or(ssid.len());
        if len == 0 {
            return None;
        }
        st.aps
            .iter()
            .find(|ap| ap.ssid.as_bytes() == &ssid[..len])
            .cloned()
    }

    /// Ends a sweep whose instant came: the world becomes the record list, then the parked caller's
    /// release or a `WIFI_EVENT_SCAN_DONE` follows, through the same [`POST_US`] hop as every other
    /// delivery.
    fn finish_scan(&self, st: &mut WifiState, now_us: u64) {
        if st.state != DriverState::Scanning || now_us < st.scan_due_us {
            return;
        }
        st.state = DriverState::Started;
        st.scan_due_us = 0;
        st.records = super::ap::heard(&st.aps);
        // A sweep that ran to its end posts `SCAN_DONE` with status 0 and its count whether or not
        // its caller blocked; the device releases a blocking caller with that very event.
        let mut data = vec![0u8; self.profile.driver.scan_done_bytes as usize];
        data[4] = u8::try_from(st.records.len()).unwrap_or(u8::MAX);
        data[5] = st.scan_id as u8;
        let mut payload = self.profile.driver.event_scan_done.to_le_bytes().to_vec();
        payload.extend_from_slice(&data);
        st.queue(now_us + POST_US, work::POST, payload);
        if st.scan_blocking {
            st.scan_blocking = false;
            st.queue(now_us + POST_US, work::WAKE, Vec::new());
        }
    }

    /// `esp_wifi_internal_tx(wifi_interface_t ifx, void *buffer, uint16_t len)`.
    ///
    /// An associated station's frame is copied out, recorded in the capture and handed to
    /// [`Lan::input`]; the answers come back through [`WifiHost::receive_frame`]. The LAN takes every
    /// frame at once, so `DYNAMIC_TX_BUFFER_NUM` is never reached. Other frames get the code of the
    /// public contract (`esp_private/wifi.h`, class B): stopped `ESP_ERR_WIFI_NOT_STARTED`, not
    /// associated `ESP_ERR_WIFI_NOT_ASSOC`, an interface the mode did not create `ESP_ERR_WIFI_CONN`,
    /// an unknown one `ESP_ERR_WIFI_IF`, empty or over the MTU `ESP_ERR_INVALID_ARG`. A frame on a
    /// created SoftAP is refused by name: there is no access-point model.
    fn tx(&self, st: &mut WifiState, g: &mut dyn GuestView) -> HleAction {
        let (ifx, buffer, len) = (g.reg(A0), g.reg(A0 + 1), g.reg(A0 + 2) & 0xFFFF);
        let inited = match self.inited(g) {
            Ok(inited) => inited,
            Err(fail) => return fail,
        };
        let max = self.rx_row().map_or(1_514, |row| row.element);
        let code = if !inited {
            Some(ESP_ERR_WIFI_NOT_INIT)
        } else if ifx > MAX_IF {
            Some(ESP_ERR_WIFI_IF)
        } else if ifx == 1 && st.mode & MODE_AP == 0 {
            Some(ESP_ERR_WIFI_CONN)
        } else if ifx == 1 {
            return HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                "wifi.tx: a frame on the SoftAP interface, which is unmodelled: the virtual LAN \
                 has a station side only",
            ));
        } else if buffer == 0 || len == 0 || len > max {
            Some(ESP_ERR_INVALID_ARG)
        } else if !st.state.started() {
            Some(ESP_ERR_WIFI_NOT_STARTED)
        } else if st.state != DriverState::Connected {
            Some(ESP_ERR_WIFI_NOT_ASSOC)
        } else {
            None
        };
        if let Some(code) = code {
            st.tx_refused += 1;
            return ret(code);
        }
        let mut frame = vec![0u8; len as usize];
        if g.read(buffer, &mut frame).is_err() {
            return hle_common::fault("wifi", "the TX frame", buffer);
        }
        let now = g.now().as_us();
        st.tx_frames += 1;
        st.capture.record(now, pcap::dir::TX, &frame);
        for answer in st.lan.input(now, &frame) {
            self.receive_frame(st, g, 0, &answer);
        }
        ret(ESP_OK)
    }

    /// `esp_wifi_internal_set_sta_ip(void)`, called by `esp_netif` once lwIP has its address. Its
    /// effect inside the blob is unverified and nothing the guest reads depends on it, so the
    /// handler records the instant and answers `ESP_OK`.
    fn set_sta_ip(&self, st: &mut WifiState, g: &mut dyn GuestView) -> HleAction {
        if st.sta_ip_us == 0 {
            st.sta_ip_us = g.now().as_us().max(1);
        }
        ret(ESP_OK)
    }

    fn step(
        &self,
        name: &str,
        state_bytes: &mut Vec<u8>,
        handler: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction {
        let mut st = match WifiState::decode(state_bytes) {
            Ok(st) => st,
            Err(err) => return HleAction::Fail(err),
        };
        let mut words = words_of(handler);
        let out = match name {
            "wifi.init" => self.init(&mut st, &mut words, g, &resume),
            "wifi.deinit" => self.deinit(&mut st, &mut words, g, &resume),
            "wifi.start" => self.start_stop(true, &mut st, &mut words, g, &resume),
            "wifi.stop" => self.start_stop(false, &mut st, &mut words, g, &resume),
            "wifi.scan_start" => self.scan_start(&mut st, &mut words, g, &resume),
            "wifi.worker" => self.worker(&mut st, &mut words, g, &resume),
            "wifi.set_mode" => self.immediate(handler::SET_MODE, &mut st, g),
            "wifi.get_mode" => self.immediate(handler::GET_MODE, &mut st, g),
            "wifi.set_storage" => self.immediate(handler::SET_STORAGE, &mut st, g),
            "wifi.set_config" => self.immediate(handler::SET_CONFIG, &mut st, g),
            "wifi.get_config" => self.immediate(handler::GET_CONFIG, &mut st, g),
            "wifi.get_mac" => self.immediate(handler::GET_MAC, &mut st, g),
            "wifi.scan_stop" => self.immediate(handler::SCAN_STOP, &mut st, g),
            "wifi.scan_get_ap_num" => self.immediate(handler::SCAN_GET_AP_NUM, &mut st, g),
            "wifi.scan_get_ap_records" => self.immediate(handler::SCAN_GET_AP_RECORDS, &mut st, g),
            "wifi.reg_rxcb" => self.immediate(handler::REG_RXCB, &mut st, g),
            "wifi.reg_netstack_buf_cb" => self.immediate(handler::REG_NETSTACK_BUF_CB, &mut st, g),
            "wifi.free_rx_buffer" => self.free_rx_buffer(&mut st, &mut words, g, &resume),
            "wifi.connect" => self.connect(&mut st, g),
            "wifi.disconnect" => self.disconnect(&mut st, &mut words, g, &resume),
            "wifi.tx" => self.tx(&mut st, g),
            "wifi.set_sta_ip" => self.set_sta_ip(&mut st, g),
            other => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                format!("the wifi module has no handler `{other}`"),
            )),
        };
        store_words(handler, &words);
        *state_bytes = st.encode();
        out
    }
}

impl ModuleHost for WifiHost {
    fn module(&self) -> ModuleIndex {
        self.module
    }

    fn name(&self) -> &'static str {
        "wifi"
    }

    fn magic_entries(&self) -> Vec<MagicKind> {
        vec![MagicKind::WifiWorker, MagicKind::WifiIsr]
    }

    fn workers(&mut self, wake: WakeMode) -> Vec<WorkerConfig> {
        self.wake = wake;
        let mut profile = wifi_profile();
        profile.stack_bytes = self.profile.worker.min_stack;
        profile.priority = self.profile.worker.priority;
        // The core turns the poll time into the guest's own ticks at each park; `poll_ticks` is
        // only the fallback for a guest whose tick rate cannot be read (20 ms at the corpus 1 kHz
        // tick).
        profile.poll_us = Some(self.profile.worker.poll_ms.saturating_mul(1_000));
        profile.poll_ticks = self.profile.worker.poll_ms;
        profile.isr_source = Some(IrqSource(self.profile.worker.isr_source));
        profile.max_scratch = Some(self.profile.worker.max_scratch);
        profile.wake = wake;
        vec![WorkerConfig {
            profile,
            calls: self.calls(),
        }]
    }

    fn enter(
        &mut self,
        state: &mut Vec<u8>,
        kind: HandlerKind,
        g: &mut dyn GuestView,
    ) -> (HandlerState, HleAction) {
        let Some(name) = handler_name(kind) else {
            return (
                HandlerState::default(),
                HleAction::Fail(HleError::new(
                    HleErrorKind::Handler,
                    format!("the wifi module has no handler {}", kind.0),
                )),
            );
        };
        let mut handler = HandlerState {
            handler: name.to_string(),
            bytes: Vec::new(),
        };
        let action = self.step(name, state, &mut handler, g, Resume::Entry);
        (handler, action)
    }

    fn resume(
        &mut self,
        state: &mut Vec<u8>,
        handler: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction {
        let name = handler.handler.clone();
        self.step(&name, state, handler, g, resume)
    }

    fn describe(&self, func: u32) -> Option<CallInfo> {
        const NAMES: [&str; 16] = [
            "esp_log",
            "esp_log_timestamp",
            "xTaskCreatePinnedToCore",
            "xQueueGenericCreate",
            "xQueueSemaphoreTake",
            "xQueueGenericSend",
            "xQueueGiveFromISR",
            "vPortYieldFromISR",
            "vTaskDelete",
            "vQueueDelete",
            "esp_event_post",
            "esp_read_mac",
            "esp_intr_alloc",
            "esp_intr_free",
            "heap_caps_malloc",
            "heap_caps_free",
        ];
        let name = NAMES.into_iter().find(|n| self.addr(n) == func)?;
        Some(CallInfo {
            name,
            // Only the FromISR calls are legal inside an ISR; every call a Wi-Fi handler makes
            // itself is from task context.
            blocking: !name.ends_with("FromISR"),
            _func: func,
        })
    }

    fn log_lines(&self, state: &[u8]) -> Option<pemu_hle::binding::RadioLogLines> {
        let synthesized = if state.is_empty() {
            0
        } else {
            WifiState::decode(state).ok()?.synthesized_lines
        };
        Some(pemu_hle::binding::RadioLogLines {
            synthesized,
            verified: self.lines_verified,
        })
    }

    fn heap_ledger(&self, state: &[u8]) -> Vec<pemu_hle::binding::HeapBlock> {
        if state.is_empty() {
            return Vec::new();
        }
        let Ok(st) = WifiState::decode(state) else {
            return Vec::new();
        };
        st.ledger.report("wifi", &self.heap_plan())
    }

    fn on_timer(
        &mut self,
        state: &mut Vec<u8>,
        tag: u16,
        g: &mut dyn GuestView,
    ) -> Vec<RadioEvent> {
        let Ok(mut st) = WifiState::decode(state) else {
            return Vec::new();
        };
        let now = g.now().as_us();
        if tag == timer::SCAN {
            self.finish_scan(&mut st, now);
        }
        if tag == timer::CONNECT {
            self.finish_connect(&mut st, g, now);
        }
        let (due, later): (Vec<Outgoing>, Vec<Outgoing>) =
            st.outbox.drain(..).partition(|o| o.due_us <= now);
        st.outbox = later;
        if let Some(next) = st.outbox.iter().map(|o| o.due_us).min() {
            self.arm(g, next, timer::DISPATCH);
        }
        *state = st.encode();
        due.into_iter()
            .map(|o| RadioEvent {
                tag: o.tag,
                payload: o.payload,
            })
            .collect()
    }

    /// The scripted air of `env`, as the `SnapValue` bytes `Machine::apply_due_journal` writes. The
    /// whole world is replaced; a running sweep takes its records when it ends. No event follows.
    fn on_input(
        &mut self,
        state: &mut Vec<u8>,
        payload: &[u8],
        _g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        let mut r = SnapReader::new(payload, "input.env.wifi_aps");
        let aps = Vec::<ScriptedAp>::snap_read(&mut r).map_err(|e| {
            HleError::new(
                HleErrorKind::Handler,
                format!("the wifi module cannot read the scripted air: {e:?}"),
            )
        })?;
        if !r.is_empty() {
            return Err(HleError::new(
                HleErrorKind::Handler,
                "the scripted air has trailing bytes",
            ));
        }
        if let Some(bad) = aps.iter().find(|ap| ap.check().is_err()) {
            return Err(HleError::new(
                HleErrorKind::Handler,
                format!(
                    "a scripted access point is out of range: {}",
                    ap_error(bad).detail()
                ),
            ));
        }
        let mut st = WifiState::decode(state)?;
        st.aps = aps;
        *state = st.encode();
        Ok(Vec::new())
    }

    /// A bridge attach or detach, or one packet from the relay's server side. The gateway's answers
    /// reach the guest through [`WifiHost::receive_frame`] like the answers to a TX frame.
    fn on_net(
        &mut self,
        state: &mut Vec<u8>,
        ev: NetInput<'_>,
        g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        let mut st = WifiState::decode(state)?;
        let frames = match ev {
            NetInput::Attach { routes } => {
                st.lan.bridge_attach(routes);
                Vec::new()
            }
            NetInput::Detach => st.lan.bridge_detach(),
            NetInput::Packet { seq, data } => st.lan.bridge_packet(seq, data),
        };
        for frame in frames {
            self.receive_frame(&mut st, g, 0, &frame);
        }
        *state = st.encode();
        Ok(Vec::new())
    }

    /// One live bridge while attached: its host peer answers in host time, so pacing is fixed at
    /// `Wall { rate: 1 }`.
    fn bridges_live(&self, state: &[u8]) -> u32 {
        if state.is_empty() {
            return 0;
        }
        WifiState::decode(state).map_or(0, |st| u32::from(st.lan.bridge.attached))
    }

    fn deliver(&mut self, state: &mut Vec<u8>, _entry: MagicKind, events: Vec<RadioEvent>) {
        if let Ok(mut st) = WifiState::decode(state) {
            for event in events {
                st.pending.push(Outgoing {
                    due_us: 0,
                    tag: event.tag,
                    payload: event.payload,
                });
            }
            *state = st.encode();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::rng::RngStream;
    use pemu_core::sched::{EventHandle, EventKey};
    use pemu_loader::elf::ElfInfo;
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};
    use pemu_rv32::trap::Trap;

    const INITED: u32 = 0x3FCA_0000;
    const EVENT_BASE_AT: u32 = 0x3C0D_0000;
    const EVENT_BASE: u32 = 0x3C0D_9000;
    const CFG: u32 = 0x3FC9_1000;
    const OUT: u32 = 0x3FC9_2000;

    #[derive(Default)]
    struct Guest {
        x: [u32; 32],
        task: u32,
        mem: BTreeMap<u32, u8>,
        now: VTime,
        sched: pemu_core::sched::Scheduler,
        armed: Vec<(u64, EventKey)>,
    }

    impl GuestView for Guest {
        fn draw_entropy(&mut self, stream: RngStream, out: &mut [u8]) {
            pemu_core::rng::DetRng::new(7)
                .stream(stream)
                .fill_bytes(out);
        }
        fn reg(&self, r: u8) -> u32 {
            self.x[usize::from(r)]
        }
        fn set_reg(&mut self, r: u8, v: u32) {
            self.x[usize::from(r)] = v;
        }
        fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Trap> {
            for (i, b) in buf.iter_mut().enumerate() {
                *b = *self.mem.get(&(addr + i as u32)).unwrap_or(&0);
            }
            Ok(())
        }
        fn write(&mut self, addr: u32, data: &[u8]) -> Result<(), Trap> {
            for (i, b) in data.iter().enumerate() {
                self.mem.insert(addr + i as u32, *b);
            }
            Ok(())
        }
        fn raise(&mut self, _s: IrqSource, _level: bool) {}
        fn schedule(&mut self, at: VTime, key: EventKey) -> EventHandle {
            self.armed.push((at.as_us(), key));
            self.sched.schedule(self.now, at, key)
        }
        fn now(&self) -> VTime {
            self.now
        }
        fn symbol(&self, _name: &str) -> Option<u32> {
            None
        }
        fn current_task(&mut self) -> u32 {
            if self.task == 0 {
                0x3FCB_0000
            } else {
                self.task
            }
        }
        fn in_isr(&mut self) -> bool {
            false
        }
        fn scheduler_running(&mut self) -> bool {
            true
        }
    }

    fn host() -> WifiHost {
        let profile = WifiProfile::load();
        let mut syms = Vec::new();
        let mut at = 0x4200_0000;
        for name in profile
            .hooks
            .iter()
            .map(|h| h.name.as_str())
            .chain(profile.calls.iter().map(String::as_str))
        {
            syms.push(Symbol {
                name: name.to_string(),
                addr: at,
                size: 4,
                kind: SymKind::Func,
                bind: SymBind::Global,
                section: SymSection::Index(1),
            });
            at += 0x100;
        }
        for (name, addr) in [("s_wifi_inited", INITED), ("WIFI_EVENT", EVENT_BASE_AT)] {
            syms.push(Symbol {
                name: name.to_string(),
                addr,
                size: 4,
                kind: SymKind::Object,
                bind: SymBind::Local,
                section: SymSection::Index(2),
            });
        }
        let elf = ElfInfo {
            sha256: [0; 32],
            entry: 0,
            sections: Vec::new(),
            segments: Vec::new(),
            symbols: SymbolTable::new(syms),
            app_desc: None,
        };
        WifiHost::new(ModuleIndex(2), &ImageView::symbols_only(&elf)).expect("host")
    }

    fn called(action: &HleAction) -> (u32, Vec<u8>, Vec<Arg>) {
        match action {
            HleAction::Call {
                func,
                scratch,
                args,
                nargs,
            } => (*func, scratch.clone(), args[..usize::from(*nargs)].to_vec()),
            other => panic!("expected a nested call, got {other:?}"),
        }
    }

    fn returned_ok(a0: u32) -> Resume {
        Resume::Returned {
            a0,
            a1: 0,
            scratch: vec![0; 8],
        }
    }

    fn drain_lines(
        h: &mut WifiHost,
        st: &mut Vec<u8>,
        hs: &mut HandlerState,
        g: &mut Guest,
        mut a: HleAction,
    ) -> (HleAction, Vec<String>) {
        let mut texts = Vec::new();
        loop {
            let func = match &a {
                HleAction::Call { func, .. } => *func,
                _ => return (a, texts),
            };
            if func == h.addr("esp_log_timestamp") {
                a = h.resume(st, hs, g, returned_ok(7));
            } else if func == h.addr("esp_log") {
                let scratch = called(&a).1;
                let text = String::from_utf8_lossy(&scratch)
                    .split('\0')
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
                    .join("|");
                texts.push(text);
                a = h.resume(st, hs, g, returned_ok(0));
            } else {
                return (a, texts);
            }
        }
    }

    fn run_start(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest) -> HandlerState {
        let (mut hs, a) = h.enter(st, HandlerKind(handler::START), g);
        let (a, _) = drain_lines(h, st, &mut hs, g, a);
        assert_eq!(
            called(&a).0,
            h.addr("xQueueSemaphoreTake"),
            "the caller parks"
        );
        hs
    }

    fn value(action: &HleAction) -> u32 {
        match action {
            HleAction::Return { a0, .. } => *a0,
            other => panic!("expected a return, got {other:?}"),
        }
    }

    fn run_init(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest) {
        let mut cfg = vec![0u8; 152];
        cfg[144..148].copy_from_slice(&0x1F2F_3F4Fu32.to_le_bytes());
        if !g.mem.contains_key(&(CFG + 52)) {
            cfg[52..56].copy_from_slice(&32u32.to_le_bytes());
        } else {
            let mut own = [0u8; 4];
            g.read(CFG + 52, &mut own).unwrap();
            cfg[52..56].copy_from_slice(&own);
        }
        g.write(CFG, &cfg).unwrap();
        g.x[usize::from(A0)] = CFG;
        let (mut hs, a) = h.enter(st, HandlerKind(handler::INIT), g);
        assert_eq!(hs.handler, "wifi.init");
        assert_eq!(called(&a).0, h.addr("xQueueGenericCreate"));
        let a = h.resume(st, &mut hs, g, returned_ok(0x1111));
        assert_eq!(called(&a).0, h.addr("xQueueGenericCreate"));
        let a = h.resume(st, &mut hs, g, returned_ok(0x2222));
        assert_eq!(called(&a).0, h.addr("xTaskCreatePinnedToCore"));
        let a = h.resume(
            st,
            &mut hs,
            g,
            Resume::Returned {
                a0: 1,
                a1: 0,
                scratch: 0x3333u32.to_le_bytes().to_vec(),
            },
        );
        assert_eq!(called(&a).0, h.addr("esp_read_mac"));
        let a = h.resume(
            st,
            &mut hs,
            g,
            Resume::Returned {
                a0: 0,
                a1: 0,
                scratch: vec![0x02, 0x00, 0x00, 0x11, 0x22, 0x33, 0, 0],
            },
        );
        let (a, lines) = drain_lines(h, st, &mut hs, g, a);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("wifi firmware version: 4df78f2")),
            "the blob's own init lines are printed: {lines:?}"
        );
        assert_eq!(value(&a), ESP_OK);
    }

    fn run_immediate(
        h: &mut WifiHost,
        st: &mut Vec<u8>,
        g: &mut Guest,
        kind: u16,
        args: [u32; 2],
    ) -> u32 {
        g.x[usize::from(A0)] = args[0];
        g.x[usize::from(A0) + 1] = args[1];
        let (_, a) = h.enter(st, HandlerKind(kind), g);
        value(&a)
    }

    #[test]
    fn init_creates_the_wifi_worker_and_marks_the_guest_inited() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.semaphore, 0x1111);
        assert_eq!(state.caller_semaphore, 0x2222);
        assert_eq!(state.worker, 0x3333);
        assert_eq!(state.state, DriverState::Init);
        assert_eq!(state.sta_mac, [0x02, 0x00, 0x00, 0x11, 0x22, 0x33]);
        assert_eq!(state.intr_handle, 0, "U5 allocates no interrupt");
        let mut inited = [0u8];
        g.read(INITED, &mut inited).unwrap();
        assert_eq!(inited, [1], "the guest's own s_wifi_inited is set");
        let before = st.clone();
        let (_, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        assert_eq!(st, before);
    }

    #[test]
    fn an_init_with_the_wrong_config_magic_is_refused() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        g.x[usize::from(A0)] = 0;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(value(&a), ESP_ERR_INVALID_ARG, "a null config");
        g.write(CFG, &[0u8; 152]).unwrap();
        g.x[usize::from(A0)] = CFG;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(value(&a), ESP_ERR_INVALID_ARG, "magic 0 is not 0x1F2F3F4F");
    }

    #[test]
    fn the_error_codes_are_the_ones_the_probe_measured() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        let (_, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_INIT, "deinit_before_init");

        run_init(&mut h, &mut st, &mut g);
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_STARTED);

        let mut hs = run_start(&mut h, &mut st, &mut g);
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );

        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 0;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(value(&a), ESP_OK, "the first scan starts");
        let scan_id = WifiState::decode(&st).unwrap().scan_id;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(
            value(&a),
            ESP_OK,
            "a second scan_start aborts and returns 0"
        );
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(
            state.scan_id, scan_id,
            "the scan_id does not move over an abort"
        );
        assert_eq!(state.state, DriverState::Scanning, "the new sweep runs");
        let aborted = state
            .outbox
            .iter()
            .find(|o| {
                o.tag == work::POST
                    && u32::from_le_bytes(o.payload[..4].try_into().unwrap())
                        == WifiProfile::load().driver.event_scan_done
            })
            .expect("the abort posts SCAN_DONE");
        assert_eq!(aborted.payload[4], 1, "status 1");
        assert_eq!(aborted.payload[8], 0, "number 0");

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        let (a, lines) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert!(
            lines.iter().any(|l| l.contains("Wi-Fi not stop")),
            "the device prints the refusal before answering: {lines:?}"
        );
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_STOPPED, "deinit_while_started");

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, lines) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert!(
            lines.iter().any(|l| l.contains("flush txq")),
            "the blob's stop lines come before the event: {lines:?}"
        );
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(called(&a).0, h.addr("vTaskDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(value(&a), ESP_OK, "esp_wifi_deinit");

        let (_, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_INIT, "deinit_again");
    }

    /// The device prints `Deinit lldesc rx mblock:10` inside `esp_wifi_deinit`, and three lines
    /// inside each `esp_wifi_stop` (`device-probe_wifi_ap-20261009T122214Z`: four stops, one
    /// deinit).
    #[test]
    fn the_lldesc_line_is_printed_by_deinit_and_not_by_stop() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, lines) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(lines.len(), 3, "a stop prints three lines: {lines:?}");
        for (line, want) in lines
            .iter()
            .zip(["flush txq", "stop sw txq", "lmac stop hw txq"])
        {
            assert!(line.contains(want), "`{want}` in its place: {lines:?}");
        }
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK,
            "RC|esp_wifi_stop|0"
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(called(&a).0, h.addr("vTaskDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        let (a, lines) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(lines.len(), 1, "a deinit prints one line: {lines:?}");
        assert!(
            lines[0].contains("Deinit lldesc rx mblock:10"),
            "the deinit line: {lines:?}"
        );
        assert_eq!(value(&a), ESP_OK, "RC|esp_wifi_deinit|0");
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Uninit,
            "the driver deinited"
        );
    }

    #[test]
    fn start_posts_sta_start_through_the_worker_before_its_caller_returns() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        let start_us = WifiProfile::load().driver.start_us();
        assert_eq!(
            g.armed.last().map(|(at, _)| *at),
            Some(start_us + POST_US),
            "the dispatch timer is armed for the queued work"
        );

        g.now = VTime::from_us(start_us + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(
            events.iter().map(|e| e.tag).collect::<Vec<_>>(),
            [work::POST, work::POST, work::WAKE],
            "both events are posted before the caller is released"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.tag == work::POST)
                .map(|e| u32::from_le_bytes(e.payload[..4].try_into().unwrap()))
                .collect::<Vec<_>>(),
            [43, 2],
            "event 43 comes before WIFI_EVENT_STA_START"
        );
        h.deliver(&mut st, MagicKind::WifiWorker, events);

        let (mut worker, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        assert_eq!(worker.handler, "wifi.worker");
        let (func, _, args) = called(&a);
        assert_eq!(func, h.addr("esp_event_post"));
        assert_eq!(
            args[0],
            Arg::Val(EVENT_BASE),
            "the base is the DROM pointer"
        );
        assert_eq!(args[1], Arg::Val(43), "the event the device posts first");
        assert_eq!(args[3], Arg::Val(0), "no event data");
        let a = h.resume(&mut st, &mut worker, &mut g, returned_ok(0));
        let (func, _, args) = called(&a);
        assert_eq!(func, h.addr("esp_event_post"));
        assert_eq!(args[1], Arg::Val(2), "WIFI_EVENT_STA_START");
        let a = h.resume(&mut st, &mut worker, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("xQueueGenericSend"), "then the caller");
        let a = h.resume(&mut st, &mut worker, &mut g, returned_ok(1));
        assert_eq!(a, HleAction::Park, "and the worker parks again");
        assert_eq!(WifiState::decode(&st).unwrap().posted_events, 2);

        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );
    }

    #[test]
    fn a_scan_dwells_in_virtual_time_and_hands_back_the_records_strongest_first() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        h.resume(&mut st, &mut hs, &mut g, returned_ok(1));
        g.now = VTime::from_us(WifiProfile::load().driver.start_us() + POST_US);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        let mut state = WifiState::decode(&st).unwrap();
        state.aps = vec![
            ap("G2-Charlie", -75, 11, 4),
            ap("G2-Alpha", -42, 1, 3),
            ap("G2-Bravo", -60, 6, 0),
        ];
        st = state.encode();

        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 0;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(value(&a), ESP_OK);
        let dwell = WifiProfile::load().driver.scan_us();
        let base = WifiProfile::load().driver.start_us() + POST_US;
        assert_eq!(dwell, 2_420_000, "the 2,420 ms sweep the device measured");
        assert_eq!(g.armed.last().map(|(at, _)| *at), Some(base + dwell));
        assert!(
            h.on_timer(&mut st, timer::SCAN, &mut g).is_empty(),
            "nothing is due before the dwell ends"
        );

        g.now = VTime::from_us(base + dwell);
        assert!(
            h.on_timer(&mut st, timer::SCAN, &mut g).is_empty(),
            "the event takes the same hop to the worker as every other delivery"
        );
        g.now = VTime::from_us(base + dwell + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(events.len(), 1, "one SCAN_DONE");
        let payload = &events[0].payload;
        assert_eq!(u32::from_le_bytes(payload[..4].try_into().unwrap()), 1);
        assert_eq!(payload[4], 0, "status 0");
        assert_eq!(payload[8], 3, "number = 3 records");
        assert_eq!(payload[9], 1, "scan_id 1");
        assert_eq!(WifiState::decode(&st).unwrap().state, DriverState::Started);

        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SCAN_GET_AP_NUM, [OUT, 0]),
            ESP_OK
        );
        let mut count = [0u8; 2];
        g.read(OUT, &mut count).unwrap();
        assert_eq!(u16::from_le_bytes(count), 3);

        g.write(OUT, &8u16.to_le_bytes()).unwrap();
        assert_eq!(
            run_immediate(
                &mut h,
                &mut st,
                &mut g,
                handler::SCAN_GET_AP_RECORDS,
                [OUT, OUT + 0x100]
            ),
            ESP_OK
        );
        g.read(OUT, &mut count).unwrap();
        assert_eq!(
            u16::from_le_bytes(count),
            3,
            "it copied 3 of the 8 asked for"
        );
        let size = WifiProfile::load().driver.ap_record_bytes as usize;
        let mut records = vec![0u8; 3 * size];
        g.read(OUT + 0x100, &mut records).unwrap();
        let ssid = |i: usize| {
            let at = i * size + 6;
            let end = at + records[at..].iter().position(|b| *b == 0).unwrap();
            String::from_utf8(records[at..end].to_vec()).unwrap()
        };
        assert_eq!(ssid(0), "G2-Alpha");
        assert_eq!(ssid(1), "G2-Bravo");
        assert_eq!(ssid(2), "G2-Charlie");
        assert_eq!(records[44] as i8, -42);
        assert!(
            WifiState::decode(&st).unwrap().records.is_empty(),
            "get_ap_records frees the list"
        );
    }

    /// An undisturbed blocking scan posts `SCAN_DONE` with status 0 like any other sweep, and that
    /// same event releases the caller with ESP_OK.
    #[test]
    fn a_blocking_scan_parks_its_caller_until_the_scan_done_it_also_posts() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        h.resume(&mut st, &mut hs, &mut g, returned_ok(1));
        g.now = VTime::from_us(WifiProfile::load().driver.start_us() + POST_US);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 1;
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        let at =
            WifiProfile::load().driver.start_us() + POST_US + WifiProfile::load().driver.scan_us();
        g.now = VTime::from_us(at);
        assert!(h.on_timer(&mut st, timer::SCAN, &mut g).is_empty());
        g.now = VTime::from_us(at + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(
            events.iter().map(|e| e.tag).collect::<Vec<_>>(),
            [work::POST, work::WAKE],
            "the sweep posts SCAN_DONE and that event releases the parked caller"
        );
        let done = &events[0].payload;
        assert_eq!(
            u32::from_le_bytes(done[..4].try_into().unwrap()),
            WifiProfile::load().driver.event_scan_done
        );
        assert_eq!(done[4], 0, "status 0: the sweep ran to its end");
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );
    }

    #[test]
    fn a_second_blocking_caller_is_refused_rather_than_parked() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        assert_eq!(
            WifiState::decode(&st).unwrap().parked_caller,
            g.current_task()
        );

        g.task = 0x3FCB_9999;
        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 1;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        match a {
            HleAction::Fail(err) => assert!(
                format!("{err:?}").contains("parked"),
                "the refusal names the parked caller: {err:?}"
            ),
            other => panic!("a second blocking caller must be refused, got {other:?}"),
        }

        g.task = 0x3FCB_0000;
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );
        assert_eq!(WifiState::decode(&st).unwrap().parked_caller, 0);
    }

    #[test]
    fn scan_stop_posts_scan_done_with_the_partial_count() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        h.resume(&mut st, &mut hs, &mut g, returned_ok(1));
        let base = WifiProfile::load().driver.start_us() + POST_US;
        g.now = VTime::from_us(base);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        let mut state = WifiState::decode(&st).unwrap();
        state.aps = vec![
            ap("G2-Alpha", -42, 1, 0),
            ap("G2-Bravo", -60, 6, 0),
            ap("G2-Charlie", -75, 11, 0),
        ];
        st = state.encode();
        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 0;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(value(&a), ESP_OK);

        g.now = VTime::from_us(base + 600_000);
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_STOP), &mut g);
        assert_eq!(value(&a), ESP_OK, "the device answers 0");
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Started, "the sweep stopped");
        assert_eq!(
            state
                .records
                .iter()
                .map(|a| a.ssid.as_str())
                .collect::<Vec<_>>(),
            ["G2-Alpha"],
            "the partial result is what the swept channels held"
        );
        let done = state
            .outbox
            .iter()
            .find(|o| {
                o.tag == work::POST
                    && u32::from_le_bytes(o.payload[..4].try_into().unwrap())
                        == WifiProfile::load().driver.event_scan_done
            })
            .expect("the stop posts SCAN_DONE");
        assert_eq!(done.payload[4], 1, "status 1");
        assert_eq!(done.payload[8], 1, "the partial number");

        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SCAN_GET_AP_NUM, [OUT, 0]),
            ESP_OK
        );
        let mut count = [0u8; 2];
        g.read(OUT, &mut count).unwrap();
        assert_eq!(u16::from_le_bytes(count), 1);
    }

    /// The stopped driver then answers 12290 to a scan query.
    #[test]
    fn stop_posts_the_aborted_sweep_scan_done_before_sta_stop() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_init(&mut h, &mut st, &mut g);
        let mut hs = run_start(&mut h, &mut st, &mut g);
        h.resume(&mut st, &mut hs, &mut g, returned_ok(1));
        let base = WifiProfile::load().driver.start_us() + POST_US;
        g.now = VTime::from_us(base);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        let mut state = WifiState::decode(&st).unwrap();
        state.aps = vec![ap("G2-Alpha", -42, 1, 0), ap("G2-Charlie", -75, 11, 0)];
        st = state.encode();
        g.x[usize::from(A0)] = 0;
        g.x[usize::from(A0) + 1] = 0;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SCAN_START), &mut g);
        assert_eq!(value(&a), ESP_OK);

        g.now = VTime::from_us(base + 600_000);
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(
            called(&a).0,
            h.addr("xQueueSemaphoreTake"),
            "the caller parks"
        );

        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Init, "the driver stopped");
        let posts: Vec<u32> = state
            .outbox
            .iter()
            .filter(|o| o.tag == work::POST)
            .map(|o| u32::from_le_bytes(o.payload[..4].try_into().unwrap()))
            .collect();
        let driver = &WifiProfile::load().driver;
        assert_eq!(
            posts,
            [driver.event_scan_done, driver.event_sta_stop],
            "the aborted sweep's SCAN_DONE comes before STA_STOP"
        );
        let done = state
            .outbox
            .iter()
            .find(|o| o.tag == work::POST)
            .expect("the SCAN_DONE");
        assert_eq!(done.payload[4], 1, "status 1: the sweep was cut short");
        assert_eq!(
            done.payload[8], 1,
            "the partial count of the swept channels"
        );

        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );
        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SCAN_GET_AP_NUM, [OUT, 0]),
            ESP_ERR_WIFI_NOT_STARTED
        );
        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SCAN_STOP, [0, 0]),
            ESP_ERR_WIFI_NOT_STARTED
        );
    }

    #[test]
    fn the_state_round_trips_and_trailing_bytes_are_refused() {
        let mut state = WifiState {
            worker: 1,
            semaphore: 2,
            caller_semaphore: 3,
            state: DriverState::Scanning,
            scan_due_us: 2_420_000,
            scan_started_us: 500,
            synthesized_lines: 5,
            ..WifiState::default()
        };
        state.aps.push(ap("G2-Alpha", -42, 1, 3));
        state.outbox.push(Outgoing {
            due_us: 7,
            tag: work::POST,
            payload: vec![1, 2, 3],
        });
        let bytes = state.encode();
        assert_eq!(WifiState::decode(&bytes).unwrap(), state);
        assert!(WifiState::decode(&[bytes.as_slice(), &[0]].concat()).is_err());
        assert_eq!(WifiState::decode(&[]).unwrap(), WifiState::default());
    }

    const RXCB: u32 = 0x4203_5000;
    const RX_HEAP: u32 = 0x3FC9_8000;
    const RX_STRIDE: u32 = 0x800;

    /// A host whose `esp_wifi_init` counts as the verified shape, which gives the module its
    /// `[[heap]]` capacity. Unit tests bind from symbols alone, where no code hash can be checked.
    fn verified_host() -> WifiHost {
        let mut h = host();
        h.lines_verified = true;
        h
    }

    fn rx_ready(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest) {
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        run_init(h, st, g);
        assert_eq!(
            run_immediate(h, st, g, handler::REG_RXCB, [0, RXCB]),
            ESP_OK,
            "the station registers its RX callback"
        );
    }

    fn deliver(
        h: &mut WifiHost,
        st: &mut Vec<u8>,
        g: &mut Guest,
        frames: &[Vec<u8>],
    ) -> Vec<(u32, u32)> {
        let mut state = WifiState::decode(st).expect("state");
        for frame in frames {
            h.receive_frame(&mut state, g, 0, frame);
        }
        *st = state.encode();
        g.now = VTime::from_us(g.now.as_us() + POST_US);
        let events = h.on_timer(st, timer::DISPATCH, g);
        h.deliver(st, MagicKind::WifiWorker, events);

        let mut lent = Vec::new();
        let mut next = RX_HEAP;
        let (mut worker, mut a) = h.enter(st, worker_handler(MagicKind::WifiWorker), g);
        loop {
            match &a {
                HleAction::Park => return lent,
                HleAction::Call { func, .. } if *func == h.addr("heap_caps_malloc") => {
                    let at = next;
                    next += RX_STRIDE;
                    a = h.resume(st, &mut worker, g, returned_ok(at));
                }
                HleAction::Call { func, args, .. } if *func == RXCB => {
                    let val = |i: usize| match args[i] {
                        Arg::Val(v) => v,
                        other => panic!("the callback takes values, got {other:?}"),
                    };
                    lent.push((val(0), val(1)));
                    a = h.resume(st, &mut worker, g, returned_ok(ESP_OK));
                }
                other => panic!("unexpected worker action {other:?}"),
            }
        }
    }

    /// The 33rd frame offered while 32 buffers are out is dropped and its buffer never asked for.
    #[test]
    fn the_thirty_third_outstanding_rx_buffer_is_dropped() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);

        let row = h.rx_row().expect("the corpus shape has an RX row");
        assert_eq!(row.count, 32, "CONFIG_ESP_WIFI_DYNAMIC_RX_BUFFER_NUM");
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(
            state.rx_capacity, row.count,
            "the capacity is the guest's own dynamic_rx_buf_num, which the row's count states"
        );
        let capacity = state.rx_capacity as usize;

        let frames: Vec<Vec<u8>> = (0..capacity + 1).map(|i| vec![i as u8; 64 + i]).collect();
        let lent = deliver(&mut h, &mut st, &mut g, &frames);

        assert_eq!(
            lent.len(),
            capacity,
            "32 frames are delivered and the 33rd is not"
        );
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.rx_delivered, capacity as u64);
        assert_eq!(state.rx_dropped, 1, "exactly the 33rd frame");
        assert_eq!(state.ledger.blocks.len(), capacity, "32 buffers are out");
        assert_eq!(
            state.ledger.refused, 0,
            "the allocator was never asked for the 33rd"
        );
        for (i, (addr, len)) in lent.iter().enumerate() {
            assert_eq!(*addr, RX_HEAP + RX_STRIDE * i as u32);
            assert_eq!(*len as usize, frames[i].len());
            assert_eq!(state.ledger.blocks[i].addr, *addr);
            assert_eq!(state.ledger.blocks[i].bytes, *len);
            let mut back = vec![0u8; frames[i].len()];
            g.read(*addr, &mut back).expect("readable");
            assert_eq!(back, frames[i], "the frame is in the buffer the guest got");
        }
        let report = h.heap_ledger(&st);
        assert_eq!(report.len(), capacity);
        assert_eq!(report[0].module, "wifi");
        assert_eq!(report[0].label, RX_ROW);
        assert_eq!(report[0].class, "C", "the count comes from a sdkconfig");
    }

    /// The capacity is a lifetime, not a budget.
    #[test]
    fn a_returned_rx_buffer_frees_its_capacity_again() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);
        let capacity = WifiState::decode(&st).expect("state").rx_capacity as usize;

        let frames: Vec<Vec<u8>> = (0..capacity).map(|_| vec![0xAA; 100]).collect();
        let lent = deliver(&mut h, &mut st, &mut g, &frames);
        assert_eq!(lent.len(), capacity);

        g.x[usize::from(A0)] = lent[0].0;
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::FREE_RX_BUFFER), &mut g);
        let (func, _, args) = called(&a);
        assert_eq!(func, h.addr("heap_caps_free"));
        assert_eq!(args[0], Arg::Val(lent[0].0), "the block it was given");
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(0))),
            ESP_OK
        );
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.ledger.blocks.len(), capacity - 1);
        assert_eq!(state.rx_freed, 1);

        let more = deliver(&mut h, &mut st, &mut g, &[vec![0xBB; 100]]);
        assert_eq!(more.len(), 1, "the freed capacity took the next frame");
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.rx_dropped, 0);
        assert_eq!(state.ledger.blocks.len(), capacity);
    }

    #[test]
    fn a_free_of_a_buffer_this_module_never_lent_frees_nothing() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);
        g.x[usize::from(A0)] = 0x3FCF_0000;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::FREE_RX_BUFFER), &mut g);
        assert_eq!(value(&a), ESP_OK, "the real call returns void");
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.rx_alien_free, 1);
        assert_eq!(state.rx_freed, 0);
    }

    #[test]
    fn a_frame_with_no_callback_or_no_buffer_that_fits_is_dropped_whole() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        run_init(&mut h, &mut st, &mut g);

        assert!(deliver(&mut h, &mut st, &mut g, &[vec![1; 64]]).is_empty());
        assert_eq!(WifiState::decode(&st).unwrap().rx_dropped, 1);
        assert!(WifiState::decode(&st).unwrap().ledger.blocks.is_empty());

        run_immediate(&mut h, &mut st, &mut g, handler::REG_RXCB, [0, RXCB]);
        let too_long = vec![2u8; h.rx_row().expect("a row").element as usize + 1];
        assert!(deliver(&mut h, &mut st, &mut g, &[too_long]).is_empty());
        assert_eq!(WifiState::decode(&st).unwrap().rx_dropped, 2);
        assert!(WifiState::decode(&st).unwrap().ledger.blocks.is_empty());
    }

    /// The refusal is counted in the ledger rather than mistaken for a full capacity.
    #[test]
    fn a_refused_allocation_drops_the_frame_and_is_counted_as_a_refusal() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);

        let mut state = WifiState::decode(&st).expect("state");
        h.receive_frame(&mut state, &mut g, 0, &[7u8; 64]);
        st = state.encode();
        g.now = VTime::from_us(g.now.as_us() + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        h.deliver(&mut st, MagicKind::WifiWorker, events);
        let (mut worker, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        assert_eq!(called(&a).0, h.addr("heap_caps_malloc"));
        assert_eq!(
            h.resume(&mut st, &mut worker, &mut g, returned_ok(0)),
            HleAction::Park,
            "nothing is handed to the callback"
        );
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.ledger.refused, 1);
        assert_eq!(state.rx_dropped, 1);
        assert_eq!(state.rx_delivered, 0);
        assert!(state.ledger.blocks.is_empty());
    }

    /// An image whose sdkconfig this build never read (`probe_wifi_http`) lends buffers too, as
    /// many as it asked for and no more.
    #[test]
    fn the_rx_capacity_is_the_guests_own_dynamic_rx_buf_num() {
        let mut h = host();
        assert!(!h.lines_verified);
        assert!(h.rx_row().is_some(), "the RX row applies to every shape");
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(CFG + 52, &4u32.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);
        assert_eq!(WifiState::decode(&st).unwrap().rx_capacity, 4);
        let frames: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 64]).collect();
        assert_eq!(deliver(&mut h, &mut st, &mut g, &frames).len(), 4);
        let state = WifiState::decode(&st).expect("state");
        assert_eq!(state.rx_dropped, 1, "the fifth frame");
        assert_eq!(state.ledger.blocks.len(), 4);
        assert_eq!(h.heap_ledger(&st).len(), 4);
    }

    #[test]
    fn a_guest_asking_for_no_rx_buffers_is_refused_by_name() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(CFG + 52, &0u32.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);
        let mut state = WifiState::decode(&st).expect("state");
        h.receive_frame(&mut state, &mut g, 0, &[1u8; 64]);
        st = state.encode();
        g.now = VTime::from_us(g.now.as_us() + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        h.deliver(&mut st, MagicKind::WifiWorker, events);
        let (_, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        match a {
            HleAction::Fail(err) => assert!(err.detail.contains("dynamic_rx_buf_num 0"), "{err}"),
            other => panic!("expected a refusal by name, got {other:?}"),
        }
    }

    /// The item stays at the head of the worker's queue until its buffer is in the ledger.
    #[test]
    fn a_save_with_an_rx_allocation_outstanding_restores_with_the_frame_still_queued() {
        let mut h = verified_host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        rx_ready(&mut h, &mut st, &mut g);

        let frame = vec![9u8; 128];
        let mut state = WifiState::decode(&st).expect("state");
        h.receive_frame(&mut state, &mut g, 0, &frame);
        st = state.encode();
        g.now = VTime::from_us(g.now.as_us() + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        h.deliver(&mut st, MagicKind::WifiWorker, events);
        let (mut worker, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        assert_eq!(called(&a).0, h.addr("heap_caps_malloc"));

        let saved = st.clone();
        let restored = WifiState::decode(&saved).expect("the state round trips");
        assert_eq!(restored.pending.len(), 1, "the frame is still queued");
        assert_eq!(restored.pending[0].tag, work::RX);
        assert_eq!(restored.pending[0].payload, {
            let mut p = vec![0u8];
            p.extend_from_slice(&frame);
            p
        });
        assert!(restored.ledger.blocks.is_empty(), "no buffer is lent yet");

        let a = h.resume(&mut st, &mut worker, &mut g, returned_ok(RX_HEAP));
        assert_eq!(called(&a).0, RXCB);
        assert_eq!(
            h.resume(&mut st, &mut worker, &mut g, returned_ok(ESP_OK)),
            HleAction::Park
        );
        assert_eq!(WifiState::decode(&st).unwrap().rx_delivered, 1);
    }

    fn ap(ssid: &str, rssi: i8, channel: u8, authmode: u8) -> ScriptedAp {
        ScriptedAp {
            ssid: ssid.to_string(),
            bssid: [0x02, 0x00, 0x00, 0x47, 0x32, channel],
            rssi,
            channel,
            authmode,
            psk: if authmode == 0 {
                Vec::new()
            } else {
                b"scripted-key".to_vec()
            },
        }
    }

    fn run_to_started(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest) -> u64 {
        run_init(h, st, g);
        let mut hs = run_start(h, st, g);
        h.resume(st, &mut hs, g, returned_ok(1));
        let base = WifiProfile::load().driver.start_us() + POST_US;
        g.now = VTime::from_us(base);
        h.on_timer(st, timer::DISPATCH, g);
        base
    }

    fn set_sta_ssid(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest, ssid: &str) {
        let mut config = vec![0u8; CONFIG_BYTES];
        config[..ssid.len()].copy_from_slice(ssid.as_bytes());
        g.write(CFG, &config).unwrap();
        assert_eq!(
            run_immediate(h, st, g, handler::SET_CONFIG, [0, CFG]),
            ESP_OK
        );
    }

    /// Returns 0 at once, then two `STA_DISCONNECTED`, reason 201 then 36, at one instant. The
    /// probe's summary line reports only the last reason, so this asserts the event stream.
    #[test]
    fn a_connect_at_an_absent_ssid_gives_two_disconnects_201_then_36() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        let base = run_to_started(&mut h, &mut st, &mut g);
        set_sta_ssid(&mut h, &mut st, &mut g, "myssid");
        let mut state = WifiState::decode(&st).unwrap();
        state.aps = vec![
            ap("G2-Alpha", -42, 1, 3),
            ap("G2-Bravo", -60, 6, 0),
            ap("G2-Charlie", -75, 11, 4),
        ];
        st = state.encode();

        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK, "RC|esp_wifi_connect|0");
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Connecting);
        let connect_us = WifiProfile::load().driver.connect_us();
        assert_eq!(connect_us, 2_420_000);
        assert_eq!(state.connect_due_us, base + connect_us);
        assert!(
            h.on_timer(&mut st, timer::CONNECT, &mut g).is_empty(),
            "nothing is due before the attempt resolves"
        );

        g.now = VTime::from_us(base + connect_us);
        assert!(
            h.on_timer(&mut st, timer::CONNECT, &mut g).is_empty(),
            "the events take the POST_US hop to the worker like every other delivery"
        );
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(
            state.state,
            DriverState::Started,
            "the attempt is over and the station is started, not associated"
        );
        assert_eq!(state.connect_due_us, 0);

        g.now = VTime::from_us(base + connect_us + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(
            events.iter().map(|e| e.tag).collect::<Vec<_>>(),
            [work::POST, work::POST],
            "two events, and no caller wake: esp_wifi_connect did not block"
        );
        let driver = &WifiProfile::load().driver;
        let ids: Vec<u32> = events
            .iter()
            .map(|e| u32::from_le_bytes(e.payload[..4].try_into().unwrap()))
            .collect();
        assert_eq!(ids, [driver.event_sta_disconnected; 2], "both are id 5");
        let reasons: Vec<u8> = events
            .iter()
            .map(|e| e.payload[4 + driver.disconnected_reason_off as usize])
            .collect();
        assert_eq!(
            reasons,
            [201, 36],
            "WIFI_REASON_NO_AP_FOUND and then WIFI_REASON_STA_LEAVING, in the capture's order"
        );
        for event in &events {
            assert_eq!(
                event.payload.len(),
                4 + driver.disconnected_bytes as usize,
                "a whole wifi_event_sta_disconnected_t"
            );
            let data = &event.payload[4..];
            assert_eq!(&data[..6], b"myssid", "the guest's own configured SSID");
            assert_eq!(data[driver.disconnected_ssid_len_off as usize], 6);
            assert_eq!(
                data[driver.disconnected_rssi_off as usize], 0,
                "no access point answered, so there is no signal to report"
            );
        }
    }

    /// `esp_wifi_disconnect` after the disconnects arrived, started and not associated, returns 0;
    /// the stop that follows posts `STA_STOP` (3).
    #[test]
    fn disconnect_answers_ok_for_a_station_that_is_not_associated_and_stop_still_posts_id_3() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        let base = run_to_started(&mut h, &mut st, &mut g);
        set_sta_ssid(&mut h, &mut st, &mut g, "myssid");
        let connect_us = WifiProfile::load().driver.connect_us();
        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        g.now = VTime::from_us(base + connect_us);
        h.on_timer(&mut st, timer::CONNECT, &mut g);
        g.now = VTime::from_us(base + connect_us + POST_US);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        let (_, a) = h.enter(&mut st, HandlerKind(handler::DISCONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK, "RC|esp_wifi_disconnect|0");
        assert!(
            WifiState::decode(&st).unwrap().outbox.is_empty(),
            "the call itself posts no event: both disconnects belonged to the attempt"
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        let queued = WifiState::decode(&st).unwrap();
        assert_eq!(
            queued
                .outbox
                .iter()
                .filter(|o| o.tag == work::POST)
                .map(|o| u32::from_le_bytes(o.payload[..4].try_into().unwrap()))
                .collect::<Vec<_>>(),
            [WifiProfile::load().driver.event_sta_stop],
            "WIFI_EVENT_STA_STOP follows the disconnect, as the capture's seq=5 does"
        );
    }

    #[derive(Default)]
    struct Sta<'a> {
        ssid: &'a str,
        password: &'a str,
        bssid_set: bool,
        channel: u8,
        threshold: u32,
    }

    fn set_sta(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest, sta: &Sta<'_>) {
        let driver = &WifiProfile::load().driver;
        let mut config = vec![0u8; CONFIG_BYTES];
        config[..sta.ssid.len()].copy_from_slice(sta.ssid.as_bytes());
        let off = driver.sta_config_password_off as usize;
        config[off..off + sta.password.len()].copy_from_slice(sta.password.as_bytes());
        config[driver.sta_config_bssid_set_off as usize] = u8::from(sta.bssid_set);
        config[driver.sta_config_channel_off as usize] = sta.channel;
        let off = driver.sta_config_threshold_authmode_off as usize;
        config[off..off + 4].copy_from_slice(&sta.threshold.to_le_bytes());
        g.write(CFG, &config).unwrap();
        assert_eq!(
            run_immediate(h, st, g, handler::SET_CONFIG, [0, CFG]),
            ESP_OK
        );
    }

    fn set_air(st: &mut Vec<u8>, aps: Vec<ScriptedAp>) {
        let mut state = WifiState::decode(st).unwrap();
        state.aps = aps;
        *st = state.encode();
    }

    fn wpa2_ap() -> ScriptedAp {
        ap("Lab-WPA2", -37, 1, 3)
    }

    fn run_to_associated(h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest) -> u64 {
        let base = run_to_started(h, st, g);
        set_sta(
            h,
            st,
            g,
            &Sta {
                ssid: "Lab-WPA2",
                password: "scripted-key",
                threshold: 3,
                ..Sta::default()
            },
        );
        set_air(st, vec![ap("Other", -50, 6, 0), wpa2_ap()]);
        let (_, a) = h.enter(st, HandlerKind(handler::CONNECT), g);
        assert_eq!(value(&a), ESP_OK, "RC|esp_wifi_connect|0");
        let at = base + WifiProfile::load().driver.associate_us();
        g.now = VTime::from_us(at);
        h.on_timer(st, timer::CONNECT, g);
        g.now = VTime::from_us(at + POST_US);
        let events = h.on_timer(st, timer::DISPATCH, g);
        h.deliver(st, MagicKind::WifiWorker, events);
        at
    }

    /// Returns 0 at once and associates 212,415 us later with one `STA_CONNECTED` carrying a whole
    /// 48-byte `wifi_event_sta_connected_t`.
    #[test]
    fn a_connect_at_a_scripted_wpa2_access_point_with_its_key_associates() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        let base = run_to_started(&mut h, &mut st, &mut g);
        set_sta(
            &mut h,
            &mut st,
            &mut g,
            &Sta {
                ssid: "Lab-WPA2",
                password: "scripted-key",
                threshold: 3,
                ..Sta::default()
            },
        );
        set_air(&mut st, vec![ap("Other", -50, 6, 0), wpa2_ap()]);

        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        let driver = &WifiProfile::load().driver;
        assert_eq!(
            driver.associate_us(),
            212_415,
            "the capture's STA_CONNECTED instant"
        );
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Connecting);
        assert_eq!(state.connect_due_us, base + 212_415);
        let peer = state.peer.expect("the attempt has its access point");
        assert!(peer.psk.is_empty(), "the association keeps no key");

        g.now = VTime::from_us(base + 212_414);
        h.on_timer(&mut st, timer::CONNECT, &mut g);
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Connecting,
            "not a microsecond early"
        );
        g.now = VTime::from_us(base + 212_415);
        assert!(h.on_timer(&mut st, timer::CONNECT, &mut g).is_empty());
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Connected
        );
        g.now = VTime::from_us(base + 212_415 + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(
            events.iter().map(|e| e.tag).collect::<Vec<_>>(),
            [work::POST],
            "one event, and no caller wake: esp_wifi_connect did not block"
        );
        let payload = &events[0].payload;
        assert_eq!(
            u32::from_le_bytes(payload[..4].try_into().unwrap()),
            driver.event_sta_connected
        );
        let data = &payload[4..];
        assert_eq!(data.len(), 48, "a whole wifi_event_sta_connected_t");
        assert_eq!(&data[..8], b"Lab-WPA2");
        assert_eq!(data[32], 8, "ssid_len");
        assert_eq!(&data[33..39], &wpa2_ap().bssid, "bssid");
        assert_eq!(data[39], 1, "channel");
        assert_eq!(
            &data[40..44],
            &3u32.to_le_bytes(),
            "authmode WIFI_AUTH_WPA2_PSK"
        );
        assert_eq!(&data[44..46], &1u16.to_le_bytes(), "aid, class C");
        assert!(
            !data.windows(12).any(|w| w == b"scripted-key"),
            "a key is never part of an event"
        );
    }

    /// Fails if the handler returns before the event is posted: the caller must park, the worker
    /// must post before the wake, and only the wake returns the caller.
    #[test]
    fn disconnect_from_the_associated_state_delivers_one_reason_8_before_the_caller_returns() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        let at = run_to_associated(&mut h, &mut st, &mut g);
        let (mut worker, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        assert_eq!(called(&a).2[1], Arg::Val(4), "STA_CONNECTED");
        assert_eq!(
            h.resume(&mut st, &mut worker, &mut g, returned_ok(0)),
            HleAction::Park
        );

        g.now = VTime::from_us(at + 3_763);
        let (mut caller, a) = h.enter(&mut st, HandlerKind(handler::DISCONNECT), &mut g);
        assert_eq!(
            called(&a).0,
            h.addr("xQueueSemaphoreTake"),
            "the caller parks: returning here would let RC|esp_wifi_disconnect|0 print before \
             the event"
        );
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Started);
        assert_eq!(state.peer, None);
        let driver = &WifiProfile::load().driver;
        assert_eq!(
            state
                .outbox
                .iter()
                .map(|o| (o.tag, word_at(&o.payload, 0)))
                .collect::<Vec<_>>(),
            [(work::POST, driver.event_sta_disconnected), (work::WAKE, 0)],
            "exactly one event, queued ahead of the wake"
        );

        g.now = VTime::from_us(at + 3_763 + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        let data = &events[0].payload[4..];
        assert_eq!(data.len(), 41);
        assert_eq!(data[39], 8, "WIFI_REASON_ASSOC_LEAVE");
        assert_eq!(&data[..8], b"Lab-WPA2");
        assert_eq!(&data[33..39], &wpa2_ap().bssid);
        assert_eq!(
            data[40] as i8, -37,
            "the scripted access point's rssi (shape)"
        );
        h.deliver(&mut st, MagicKind::WifiWorker, events);

        let a = h.resume(
            &mut st,
            &mut worker,
            &mut g,
            Resume::Woken {
                reason: WakeReason::Event,
            },
        );
        let (func, _, args) = called(&a);
        assert_eq!(func, h.addr("esp_event_post"), "the event first");
        assert_eq!(args[1], Arg::Val(driver.event_sta_disconnected));
        let a = h.resume(&mut st, &mut worker, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("xQueueGenericSend"), "then the caller");
        assert_eq!(
            h.resume(&mut st, &mut worker, &mut g, returned_ok(1)),
            HleAction::Park
        );
        assert_eq!(
            value(&h.resume(&mut st, &mut caller, &mut g, returned_ok(1))),
            ESP_OK,
            "RC|esp_wifi_disconnect|0"
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        assert_eq!(
            WifiState::decode(&st)
                .unwrap()
                .outbox
                .iter()
                .filter(|o| o.tag == work::POST)
                .map(|o| word_at(&o.payload, 0))
                .collect::<Vec<_>>(),
            [driver.event_sta_stop],
            "no second disconnect: the station already left"
        );
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK,
            "RC|esp_wifi_stop|0"
        );
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(called(&a).0, h.addr("vTaskDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, returned_ok(0));
        assert_eq!(value(&a), ESP_OK, "RC|esp_wifi_deinit|0");
    }

    /// The gateway's answer is queued for the worker rather than handed to the guest inside the
    /// call.
    #[test]
    fn tx_refuses_by_the_public_codes_and_sends_an_associated_frame_to_the_lan() {
        const FRAME: u32 = 0x3FC9_3000;
        let sta = [0x02, 0x00, 0x00, 0x11, 0x22, 0x33];
        let mut arp = vec![0xFF; 6];
        arp.extend_from_slice(&sta);
        arp.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 1]);
        arp.extend_from_slice(&sta);
        arp.extend_from_slice(&[10, 23, 0, 100]);
        arp.extend_from_slice(&[0; 6]);
        arp.extend_from_slice(&crate::lan::gateway::GATEWAY_IP);
        assert_eq!(arp.len(), 42);
        let tx = |h: &mut WifiHost, st: &mut Vec<u8>, g: &mut Guest, ifx: u32, len: u32| {
            g.x[usize::from(A0)] = ifx;
            g.x[usize::from(A0) + 1] = FRAME;
            g.x[usize::from(A0) + 2] = len;
            let (_, a) = h.enter(st, HandlerKind(handler::TX), g);
            value(&a)
        };

        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        g.write(FRAME, &arp).unwrap();
        let mut st = Vec::new();
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 42), ESP_ERR_WIFI_NOT_INIT);

        run_init(&mut h, &mut st, &mut g);
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 42), ESP_ERR_WIFI_NOT_STARTED);
        assert_eq!(tx(&mut h, &mut st, &mut g, 2, 42), ESP_ERR_WIFI_IF);
        assert_eq!(tx(&mut h, &mut st, &mut g, 1, 42), ESP_ERR_WIFI_CONN);
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 0), ESP_ERR_INVALID_ARG);
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 4_000), ESP_ERR_INVALID_ARG);
        let state = WifiState::decode(&st).unwrap();
        // Six: the one before the init counts too, since an init does not clear the counters.
        assert_eq!((state.tx_frames, state.tx_refused), (0, 6));
        assert!(
            state.capture.records.is_empty(),
            "a refused frame is not captured"
        );

        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        g.write(FRAME, &arp).unwrap();
        let mut st = Vec::new();
        run_to_started(&mut h, &mut st, &mut g);
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 42), ESP_ERR_WIFI_NOT_ASSOC);

        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        g.write(FRAME, &arp).unwrap();
        let mut st = Vec::new();
        run_to_associated(&mut h, &mut st, &mut g);
        assert_eq!(tx(&mut h, &mut st, &mut g, 0, 42), ESP_OK);
        let state = WifiState::decode(&st).unwrap();
        assert_eq!((state.tx_frames, state.tx_refused), (1, 0));
        assert_eq!(
            state.capture.records.len(),
            1,
            "the TX frame, and no RX yet"
        );
        assert_eq!(state.capture.records[0].dir, pcap::dir::TX);
        assert_eq!(state.capture.records[0].frame, arp);
        assert_eq!(state.lan.counters.arp_replies, 1);
        assert_eq!(state.rx_delivered, 0, "no answer inside the call");
        let rx: Vec<&Outgoing> = state.outbox.iter().filter(|o| o.tag == work::RX).collect();
        assert_eq!(rx.len(), 1, "the ARP reply waits for the worker");
        assert_eq!(rx[0].due_us, g.now.as_us() + POST_US);
        assert_eq!(rx[0].payload[0], 0, "on WIFI_IF_STA");
        assert_eq!(&rx[0].payload[1..7], &sta, "to the station");
        assert_eq!(&rx[0].payload[13..15], &[0x08, 0x06], "an ARP frame");
    }

    #[test]
    fn set_sta_ip_keeps_its_first_instant() {
        let mut h = host();
        let mut g = Guest::default();
        let mut st = Vec::new();
        g.now = VTime::from_us(1_234);
        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SET_STA_IP, [0, 0]),
            ESP_OK
        );
        g.now = VTime::from_us(9_999);
        assert_eq!(
            run_immediate(&mut h, &mut st, &mut g, handler::SET_STA_IP, [0, 0]),
            ESP_OK
        );
        assert_eq!(WifiState::decode(&st).unwrap().sta_ip_us, 1_234);
    }

    /// What `probe_wifi_http` does after its GET: one reason-8 `STA_DISCONNECTED` queued ahead of
    /// `STA_STOP`, and 0 after both.
    #[test]
    fn stop_of_an_associated_station_leaves_with_reason_8_before_sta_stop() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_to_associated(&mut h, &mut st, &mut g);
        let (mut worker, a) = h.enter(&mut st, worker_handler(MagicKind::WifiWorker), &mut g);
        assert_eq!(called(&a).2[1], Arg::Val(4), "STA_CONNECTED");
        assert_eq!(
            h.resume(&mut st, &mut worker, &mut g, returned_ok(0)),
            HleAction::Park
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(
            called(&a).0,
            h.addr("xQueueSemaphoreTake"),
            "the caller parks"
        );
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Init);
        assert_eq!(state.peer, None);
        let driver = &WifiProfile::load().driver;
        let queued: Vec<(u16, u32)> = state
            .outbox
            .iter()
            .map(|o| (o.tag, word_at(&o.payload, 0)))
            .collect();
        assert_eq!(
            queued,
            [
                (work::POST, driver.event_sta_disconnected),
                (work::POST, driver.event_sta_stop),
                (work::WAKE, 0)
            ],
            "the leave, then STA_STOP, then the caller"
        );
        let leave = &state.outbox[0].payload[4..];
        assert_eq!(leave[39], 8, "WIFI_REASON_ASSOC_LEAVE");
        assert_eq!(
            value(&h.resume(&mut st, &mut hs, &mut g, returned_ok(1))),
            ESP_OK
        );
    }

    /// No refusal carries the key or the password.
    #[test]
    fn what_the_capture_did_not_exercise_is_refused_by_name() {
        let refused = |sta: Sta<'_>, air: ScriptedAp, why: &str| {
            let mut h = host();
            h.workers(WakeMode::U5Polling);
            let mut g = Guest::default();
            g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
            let mut st = Vec::new();
            run_to_started(&mut h, &mut st, &mut g);
            set_sta(&mut h, &mut st, &mut g, &sta);
            set_air(&mut st, vec![air]);
            let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
            let HleAction::Fail(err) = &a else {
                panic!("{why}: expected a refusal, got {a:?}");
            };
            let text = format!("{err:?}");
            assert!(text.contains(why), "{why}: {text}");
            assert!(text.contains(ASSOC_CAPTURE), "{text}");
            assert!(
                !text.contains("scripted-key") && !text.contains("wrong-key"),
                "{text}"
            );
            assert_eq!(
                WifiState::decode(&st).unwrap().state,
                DriverState::Started,
                "{why}: a refused connect starts nothing"
            );
        };
        let sta = |password, threshold| Sta {
            ssid: "Lab-WPA2",
            password,
            threshold,
            ..Sta::default()
        };
        refused(sta("wrong-key", 3), wpa2_ap(), "password is not");
        refused(sta("", 3), ap("Lab-WPA2", -37, 1, 0), "cannot meet");
        refused(
            sta("scripted-key", 0),
            ap("Lab-WPA2", -37, 1, 4),
            "auth mode is 4",
        );
        refused(
            sta("scripted-key", 0),
            ap("Lab-WPA2", -37, 1, 1),
            "auth mode is 1",
        );
        let mut open = ap("Lab-WPA2", -37, 1, 0);
        open.psk.clear();
        refused(sta("scripted-key", 0), open, "open access point");
        refused(
            Sta {
                bssid_set: true,
                ..sta("scripted-key", 3)
            },
            wpa2_ap(),
            "pins a BSSID",
        );
        refused(
            Sta {
                channel: 1,
                ..sta("scripted-key", 3)
            },
            wpa2_ap(),
            "pins channel 1",
        );

        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        run_to_associated(&mut h, &mut st, &mut g);
        for (kind, why) in [
            (handler::CONNECT, "already associated"),
            (handler::SCAN_START, "associated or associating"),
        ] {
            let (_, a) = h.enter(&mut st, HandlerKind(kind), &mut g);
            let HleAction::Fail(err) = &a else {
                panic!("{why}: expected a refusal, got {a:?}");
            };
            assert!(format!("{err:?}").contains(why), "{err:?}");
        }
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Connected
        );
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_STOPPED);
    }

    /// `probe_wifi_http` joins an open access point with no password and the default
    /// `WIFI_AUTH_OPEN` threshold.
    #[test]
    fn an_open_scripted_access_point_associates_with_the_capture_timing() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        let base = run_to_started(&mut h, &mut st, &mut g);
        set_sta(
            &mut h,
            &mut st,
            &mut g,
            &Sta {
                ssid: "passport-emu-virtual-ap",
                ..Sta::default()
            },
        );
        set_air(&mut st, vec![ap("passport-emu-virtual-ap", -40, 6, 0)]);
        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        g.now = VTime::from_us(base + 212_415);
        h.on_timer(&mut st, timer::CONNECT, &mut g);
        g.now = VTime::from_us(base + 212_415 + POST_US);
        let events = h.on_timer(&mut st, timer::DISPATCH, &mut g);
        assert_eq!(events.len(), 1);
        let data = &events[0].payload[4..];
        assert_eq!(data[39], 6, "channel");
        assert_eq!(
            &data[40..44],
            &0u32.to_le_bytes(),
            "authmode WIFI_AUTH_OPEN"
        );
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Connected
        );
    }

    /// `FORMAT_VERSION` 15.
    #[test]
    fn an_association_survives_the_module_state_round_trip() {
        let state = WifiState {
            state: DriverState::Connected,
            peer: Some(ScriptedAp {
                psk: Vec::new(),
                ..wpa2_ap()
            }),
            config: vec![vec![0u8; CONFIG_BYTES]],
            ..WifiState::default()
        };
        let back = WifiState::decode(&state.encode()).expect("the module state decodes");
        assert_eq!(back, state);
        let mut byte = Vec::new();
        DriverState::Connected.snap_write(&mut byte);
        assert_eq!(
            byte,
            [5],
            "the sixth state byte, which a format 14 reader refuses"
        );
        assert!(
            DriverState::snap_read(&mut SnapReader::new(&[6], "hle.machine")).is_err(),
            "an unknown state byte is refused"
        );
    }

    #[test]
    fn connect_and_disconnect_carry_the_real_error_codes() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(EVENT_BASE_AT, &EVENT_BASE.to_le_bytes()).unwrap();
        let mut st = Vec::new();
        for kind in [handler::CONNECT, handler::DISCONNECT] {
            let (_, a) = h.enter(&mut st, HandlerKind(kind), &mut g);
            assert_eq!(value(&a), ESP_ERR_WIFI_NOT_INIT);
        }
        run_init(&mut h, &mut st, &mut g);
        for kind in [handler::CONNECT, handler::DISCONNECT] {
            let (_, a) = h.enter(&mut st, HandlerKind(kind), &mut g);
            assert_eq!(value(&a), ESP_ERR_WIFI_NOT_STARTED);
        }
        let mut hs = run_start(&mut h, &mut st, &mut g);
        h.resume(&mut st, &mut hs, &mut g, returned_ok(1));
        g.now = VTime::from_us(WifiProfile::load().driver.start_us() + POST_US);
        h.on_timer(&mut st, timer::DISPATCH, &mut g);

        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        assert_eq!(
            WifiState::decode(&st).unwrap().state,
            DriverState::Connecting
        );
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(value(&a), ESP_ERR_WIFI_NOT_STOPPED);

        let (_, a) = h.enter(&mut st, HandlerKind(handler::DISCONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.state, DriverState::Started);
        assert_eq!(state.connect_due_us, 0);
        assert!(state.outbox.iter().all(|o| o.tag != work::POST));

        let (_, a) = h.enter(&mut st, HandlerKind(handler::CONNECT), &mut g);
        assert_eq!(value(&a), ESP_OK);
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::STOP), &mut g);
        let (a, _) = drain_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(called(&a).0, h.addr("xQueueSemaphoreTake"));
        let state = WifiState::decode(&st).unwrap();
        assert_eq!(state.connect_due_us, 0);
        assert_eq!(state.state, DriverState::Init);
    }

    /// A snapshot between the connect and its disconnects restores to the same instant.
    #[test]
    fn an_outstanding_connect_attempt_survives_the_module_state_round_trip() {
        let mut state = WifiState {
            state: DriverState::Connecting,
            connect_due_us: 2_459_500,
            ..WifiState::default()
        };
        state.config = vec![vec![0u8; CONFIG_BYTES]];
        let bytes = state.encode();
        let back = WifiState::decode(&bytes).expect("the module state decodes");
        assert_eq!(back, state);
        assert_eq!(back.state, DriverState::Connecting);
        assert_eq!(back.connect_due_us, 2_459_500);
    }
}
