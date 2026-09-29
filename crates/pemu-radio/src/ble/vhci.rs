//! The 7 VHCI handlers and the `btController` worker, as the BLE module's `ModuleHost`.
//!
//! Each handler implements the observable contract of its IDF 5.5.3 `bt.c` function:
//!
//! | Function | Contract |
//! |---|---|
//! | `esp_bt_controller_init(cfg)` | IDLE, else `ESP_ERR_INVALID_STATE`; `cfg` non-null, priority 23 and stack of at least 4,096, else `ESP_ERR_INVALID_ARG`; BLE-only mode, else `ESP_ERR_NOT_SUPPORTED`; `1 <= ble_max_act <= 10`, else `ESP_ERR_INVALID_ARG`; writes the config magic; prints the `init` log lines; creates the worker semaphore and task; allocates the radio interrupt (source 8, level 3) with the magic ISR in both wake modes; takes the `init` and not yet held `boot` heap-ledger rows; spends `ble_init_ps`; status INITED |
//! | `esp_bt_controller_deinit()` | INITED, else `ESP_ERR_INVALID_STATE`; gives back the `init` ledger blocks newest first (`boot` blocks stay); frees the interrupt; deletes the worker and its semaphore; drops undelivered packets; spends `ble_deinit_ps`; status IDLE |
//! | `esp_bt_controller_enable(mode)` | INITED, else `ESP_ERR_INVALID_STATE`; mode BLE, else `ESP_ERR_INVALID_ARG`; takes the `enable` ledger rows; prints the `enable` lines once per boot; spends `ble_enable_ps`, plus `ble_enable_nvs_cal_ps` when the image links [`NVS_CALIBRATION_SYMBOL`]; status ENABLED |
//! | `esp_bt_controller_disable()` | ENABLED, else `ESP_ERR_INVALID_STATE`; gives back the `enable` ledger blocks; spends `ble_disable_ps`; status INITED |
//! | `esp_vhci_host_check_send_available()` | true only while ENABLED |
//! | `esp_vhci_host_send_packet(data, len)` | ignored unless ENABLED; over 259 bytes (`MAX_H4`) refused whole and counted; the packet goes to the controller, then `notify_host_send_available` re-arms the host's send semaphore |
//! | `esp_vhci_host_register_callback(cb)` | `ESP_FAIL` unless ENABLED; stores the two callback pointers |
//!
//! The status lives in the guest's own `btdm_controller_status`, so the real
//! `esp_bt_controller_get_status` agrees and a snapshot carries it in RAM. The init order, the
//! ledger, the spent times and the answer delays follow the probe capture
//! `device-probe_campaign_radio` (`ble.toml`, `specs/timing-profiles.toml`). A spent time is a
//! nested ROM `ets_delay_us`, so the calling task is busy as on silicon.
//!
//! Controller events wait in [`BleState::outbox`] until `reply_us` later under a module timer;
//! the worker then passes each to `notify_host_recv` in task context, so an answer is never a
//! synchronous wait.

use std::collections::BTreeMap;

use pemu_core::irq_source::IrqSource;
use pemu_core::snap::{SnapReader, SnapValue, snap_struct};
use pemu_hle::binding::ImageView;
use pemu_hle::continuation::{HandlerState, Resume};
use pemu_hle::core::{BusyDelays, CallInfo, HciInput, ModuleHost, worker_handler};
use pemu_hle::guest_call::{Arg, CallRequest, GuestView, HleAction, HleError, HleErrorKind};
use pemu_hle::hooks::{HandlerKind, ModuleIndex};
use pemu_hle::log_synth::{LogLevel, LogLineTemplate, LogSynth};
use pemu_hle::magic::{MagicKind, MagicPcs};
use pemu_hle::worker::{
    RadioEvent, WakeMode, WakeReason, WorkerCalls, WorkerConfig, bt_controller_profile,
};

use pemu_core::rng::RngStream;
use pemu_core::time::VTime;
use pemu_hle::core::module_timer;

use super::air::{self, Air, AirOut};
use super::btsnoop::{self, Capture};
use super::central::{self, Central};
use super::controller::Controller;
use super::profile::BleProfile;
use crate::heap_ledger::{LEDGER_CAPS, Ledger, LedgerRow, Lifetime};
use crate::hle_common::{self, bad_step, ret, returned, store_words, word_at, words_of};
use crate::log_lines::{LogLines, Stage};

/// `esp_bt_controller_status_t` of `esp_bt.h`.
pub const STATUS_IDLE: u32 = 0;
pub const STATUS_INITED: u32 = 1;
pub const STATUS_ENABLED: u32 = 2;

pub const ESP_OK: u32 = 0;
pub const ESP_FAIL: u32 = u32::MAX;
pub const ESP_ERR_NO_MEM: u32 = 0x101;
pub const ESP_ERR_INVALID_ARG: u32 = 0x102;
pub const ESP_ERR_INVALID_STATE: u32 = 0x103;
pub const ESP_ERR_NOT_SUPPORTED: u32 = 0x106;

/// `ESP_BT_CTRL_CONFIG_MAGIC_VAL` of `esp_bt.h`.
pub const CONFIG_MAGIC: u32 = 0x5A5A_A5A5;
pub const MODE_BLE: u32 = 1;
/// `BT_CTRL_BLE_MAX_ACT_LIMIT`.
pub const MAX_ACT_LIMIT: u8 = 10;
pub const ESP_MAC_BT: u32 = 2;
/// The level `bt.c`'s `interrupt_alloc_wrapper` asks for. Not `ESP_INTR_FLAG_IRAM`, which the
/// wrapper adds too: the allocator requires an IRAM-flagged handler to lie in IRAM, and the magic
/// ISR pc is a ROM address. That flag does not change which line is chosen.
pub const INTR_FLAG_LEVEL3: u32 = 1 << 3;

/// Byte offsets inside the ESP32-C3 `esp_bt_controller_config_t` (`esp_bt.h`).
mod cfg_off {
    pub const MAGIC: u32 = 0;
    pub const STACK: u32 = 8;
    pub const PRIO: u32 = 10;
    pub const CPU: u32 = 11;
    pub const MODE: u32 = 12;
    pub const MAX_ACT: u32 = 13;
}

pub const RECENT_TX: usize = 16;
/// The largest H4 packet: an ACL header of 5 bytes and a 251-byte payload, or a command of 4
/// bytes and 255 parameters.
pub const MAX_H4: u32 = 259;

/// Handler numbers, the `handler` column of `ble.toml`.
pub mod handler {
    pub const INIT: u16 = 0;
    pub const DEINIT: u16 = 1;
    pub const ENABLE: u16 = 2;
    pub const DISABLE: u16 = 3;
    pub const CHECK_SEND: u16 = 4;
    pub const SEND_PACKET: u16 = 5;
    pub const REGISTER_CB: u16 = 6;
}

/// The BLE module's runtime state, kept as bytes in the `hle.machine` section.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BleState {
    pub worker: u32,
    pub semaphore: u32,
    /// The init's `esp_intr_alloc` handle, 0 while none is held.
    pub intr_handle: u32,
    pub cb_send_available: u32,
    pub cb_recv: u32,
    /// True once this boot printed the `enable` lines.
    pub phy_logged: bool,
    pub synthesized_lines: u32,
    pub tx_packets: u64,
    pub rx_packets: u64,
    /// Handed to the worker and not yet delivered, oldest first.
    pub pending_rx: Vec<Vec<u8>>,
    /// Dropped because no receive callback was registered.
    pub dropped_rx: u64,
    /// First four bytes (H4 type and header) of each recent host packet, newest last.
    pub recent_tx: Vec<Vec<u8>>,
    pub refused_tx: u64,
    /// The `esp_read_mac(ESP_MAC_BT)` bytes in printed order; the public address is their reverse.
    pub bt_mac: [u8; 6],
    pub controller: Controller,
    pub outbox: Vec<Outgoing>,
    pub air: Air,
    pub central: Central,
    pub capture: Capture,
    /// Guest-heap blocks the replaced controller blob would have held.
    pub ledger: Ledger,
    pub external: External,
}

/// The external HCI bridge. While attached, the guest's packets go to [`External::outbound`] for
/// an external controller instead of the virtual one, and its packets come back as journaled
/// `InputEvent::HciPacket` inputs. Inbound packets are delivered whether or not it is attached.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct External {
    pub attached: bool,
    /// The last [`EXTERNAL_OUT_MAX`] guest packets, oldest first. Readers use a cursor and never
    /// drain it, so taking packets out changes no guest state and a snapshot at any instant
    /// restores equal.
    pub outbound: Vec<Vec<u8>>,
    /// Packets sent since attach, including those the window dropped: the absolute cursor base.
    pub sent: u64,
    /// Packets that left the window unread. The guest never blocks on a slow reader, so a stalled
    /// peer loses packets and this counts them.
    pub dropped_out: u64,
    pub inbound: u64,
    pub next_seq: u64,
    /// Packets the external stream lost, from the gaps in `seq`.
    pub lost_in: u64,
}

/// The controller's 12 ACL buffers plus room for a command queue, so a peer one round trip behind
/// never loses a packet.
pub const EXTERNAL_OUT_MAX: usize = 64;

impl External {
    pub fn window_start(&self) -> u64 {
        self.sent - self.outbound.len() as u64
    }

    /// The packets at or after absolute position `cursor`, and the next cursor. A caller whose
    /// cursor fell behind the window sees the jump in the returned position; a cursor past the end
    /// (kept across a restore) reads nothing.
    pub fn since(&self, cursor: u64) -> (Vec<Vec<u8>>, u64) {
        let skip = (cursor.max(self.window_start()) - self.window_start()) as usize;
        let skip = skip.min(self.outbound.len());
        (self.outbound[skip..].to_vec(), self.sent)
    }
}

snap_struct!(External {
    attached,
    outbound,
    sent,
    dropped_out,
    inbound,
    next_seq,
    lost_in
});

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outgoing {
    pub due_us: u64,
    pub packet: Vec<u8>,
}

snap_struct!(Outgoing { due_us, packet });

pub const TIMER_CONTROLLER: u16 = 1;

snap_struct!(BleState {
    worker,
    semaphore,
    intr_handle,
    cb_send_available,
    cb_recv,
    phy_logged,
    synthesized_lines,
    tx_packets,
    rx_packets,
    pending_rx,
    dropped_rx,
    recent_tx,
    refused_tx,
    bt_mac,
    controller,
    outbox,
    air,
    central,
    capture,
    ledger,
    external,
});

impl BleState {
    /// Empty bytes are a fresh state (a machine before the first hook).
    pub fn decode(bytes: &[u8]) -> Result<BleState, HleError> {
        if bytes.is_empty() {
            return Ok(BleState::default());
        }
        let mut r = SnapReader::new(bytes, "hle.machine");
        let state = BleState::snap_read(&mut r)
            .map_err(|e| HleError::new(HleErrorKind::Handler, format!("ble state: {e:?}")))?;
        if !r.is_empty() {
            return Err(HleError::new(
                HleErrorKind::Handler,
                "ble state has trailing bytes",
            ));
        }
        Ok(state)
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.snap_write(&mut out);
        out
    }
}

/// Whether `state` keeps key material in its btsnoop capture, which makes the instance
/// secret-bearing. Undecodable bytes answer false: such a state is refused everywhere else long
/// before an export.
pub fn capture_keeps_secrets(state: &[u8]) -> bool {
    BleState::decode(state).is_ok_and(|st| st.capture.secrets)
}

fn handler_name(kind: HandlerKind) -> Option<&'static str> {
    Some(match kind.0 {
        handler::INIT => "ble.init",
        handler::DEINIT => "ble.deinit",
        handler::ENABLE => "ble.enable",
        handler::DISABLE => "ble.disable",
        handler::CHECK_SEND => "ble.check_send_available",
        handler::SEND_PACKET => "ble.send_packet",
        handler::REGISTER_CB => "ble.register_callback",
        _ if kind == worker_handler(MagicKind::BtWorker) => "ble.worker",
        _ => return None,
    })
}

/// Steps of the handlers that make nested calls; a step names the call whose return it awaits.
mod step {
    pub const ENTRY: u32 = 0;
    pub const SEMAPHORE: u32 = 1;
    pub const TASK: u32 = 2;
    pub const INTR: u32 = 3;
    pub const MAC: u32 = 4;
    pub const TIMESTAMP: u32 = 5;
    pub const LOGGED: u32 = 6;
    pub const DELETE_TASK: u32 = 7;
    pub const DELETE_QUEUE: u32 = 8;
    pub const NOTIFIED: u32 = 9;
    pub const PARKED: u32 = 10;
    pub const DELIVERED: u32 = 11;
    pub const INTR_FREED: u32 = 12;
    pub const LEDGER_ALLOC: u32 = 13;
    pub const LEDGER_FREE: u32 = 14;
    pub const INIT_DELAY: u32 = 15;
    pub const ENABLE_DELAY: u32 = 16;
    pub const DISABLE_DELAY: u32 = 17;
    pub const DEINIT_DELAY: u32 = 18;
}

/// Linked only when the image keeps PHY calibration in NVS
/// (`CONFIG_ESP_PHY_CALIBRATION_AND_DATA_STORAGE`, IDF `esp_phy_load_cal_and_init`); with the
/// option off the linker drops it.
pub const NVS_CALIBRATION_SYMBOL: &str = "esp_phy_load_cal_data_from_nvs";

/// Word layout of the init and enable handler states.
mod w {
    pub const STEP: usize = 0;
    pub const LINE: usize = 1;
    pub const CFG: usize = 2;
    pub const STACK: usize = 3;
    pub const PRIO: usize = 4;
    pub const CPU: usize = 5;
    pub const MAC_LO: usize = 6;
    pub const MAC_HI: usize = 7;
    /// Index of the next heap-ledger row to allocate, or to free counting down.
    pub const LEDGER: usize = 8;
    pub const LEN: usize = 9;
}

pub struct BleHost {
    module: ModuleIndex,
    profile: &'static BleProfile,
    lines: &'static LogLines,
    pcs: MagicPcs,
    addrs: BTreeMap<String, u32>,
    synth: LogSynth,
    /// Whether the bound init matched a shape whose sdkconfig-dependent lines are verified.
    lines_verified: bool,
    nvs_calibration: bool,
    /// The ROM busy-wait `ets_delay_us` the handler busy time is spent in, or 0 without ROM
    /// symbols.
    delay_fn: u32,
    /// The blob's own work per handler, which the HLE otherwise does in zero time. Init spends it
    /// after its last line, enable after its `phy_init` line, disable and deinit before they
    /// return.
    delays: BusyDelays,
}

impl BleHost {
    /// `None` when the image does not link the controller or lacks a symbol the handlers need.
    pub fn new(module: ModuleIndex, image: &ImageView<'_>) -> Option<BleHost> {
        let profile = BleProfile::load();
        let symbols = &image.elf.symbols;
        symbols.addr_of(&profile.hooks.first()?.name)?;
        let addrs = hle_common::symbol_addrs(symbols, &profile.calls, &profile.data)?;
        let lines = LogLines::load();
        let synth = LogSynth::new(
            addrs.get("esp_log").copied(),
            addrs.get("esp_log_timestamp").copied(),
            &lines.info_format,
        )
        .ok()?;
        let lines_verified = hle_common::init_lines_verified(image, profile.hooks.first()?);
        Some(BleHost {
            module,
            profile,
            lines,
            synth,
            lines_verified,
            pcs: MagicPcs::from_spec()?,
            addrs,
            nvs_calibration: symbols.addr_of(NVS_CALIBRATION_SYMBOL).is_some(),
            delay_fn: image
                .rom
                .and_then(|rom| rom.addr_of("ets_delay_us"))
                .unwrap_or(0),
            delays: BusyDelays::default(),
        })
    }

    fn enable_delay_us(&self) -> u32 {
        let extra = if self.nvs_calibration {
            self.delays.enable_nvs_cal_us
        } else {
            0
        };
        self.delays.enable_us.saturating_add(extra)
    }

    fn addr(&self, name: &str) -> u32 {
        self.addrs.get(name).copied().unwrap_or(0)
    }

    fn calls(&self) -> WorkerCalls {
        hle_common::worker_calls(&self.addrs)
    }

    fn status(&self, g: &mut dyn GuestView) -> Result<u32, HleAction> {
        let at = self.addr("btdm_controller_status");
        pemu_hle::guest_call::read_u32(g, at)
            .map_err(|_| hle_common::fault("ble", "btdm_controller_status", at))
    }

    fn set_status(&self, g: &mut dyn GuestView, value: u32) -> Result<(), HleAction> {
        let at = self.addr("btdm_controller_status");
        g.write(at, &value.to_le_bytes())
            .map_err(|_| hle_common::fault("ble", "btdm_controller_status", at))
    }

    fn timestamp_call(&self) -> HleAction {
        hle_common::timestamp_call(&self.synth, "ble")
    }

    fn log_call(&self, tag: &str, text: &str, timestamp: u32) -> HleAction {
        let line = LogLineTemplate::new(LogLevel::Info, tag, text);
        match self.synth.call(&line, timestamp) {
            Some(call) => HleAction::from(call),
            None => hle_common::missing_log_functions("ble"),
        }
    }

    fn next_line(&self, stage: Stage, words: &mut [u32]) -> Option<HleAction> {
        self.lines
            .stage(stage, self.lines_verified)
            .nth(words[w::LINE] as usize)?;
        words[w::STEP] = step::TIMESTAMP;
        Some(self.timestamp_call())
    }

    fn line_text(&self, stage: Stage, words: &[u32]) -> Option<(String, String)> {
        let line = self
            .lines
            .stage(stage, self.lines_verified)
            .nth(words[w::LINE] as usize)?;
        let mac = words[w::MAC_LO].to_le_bytes();
        let hi = words[w::MAC_HI].to_le_bytes();
        let mac = hle_common::mac_text(&[mac[0], mac[1], mac[2], mac[3], hi[0], hi[1]]);
        Some((line.tag.clone(), line.text.replace("{mac}", &mac)))
    }
}

impl BleHost {
    fn to_controller(&self, st: &mut BleState, g: &mut dyn GuestView, packet: &[u8]) {
        let now = g.now().as_us();
        st.capture.record(now, btsnoop::dir::TO_CONTROLLER, packet);
        let address = st.public_address();
        let events = st
            .controller
            .on_packet(address, packet, &mut |out: &mut [u8]| {
                g.draw_entropy(RngStream::RADIO_BLE, out)
            });
        // An HCI_Reset is answered later than any other packet, as on the device.
        let reply_us = if packet.starts_with(&[0x01, 0x03, 0x0C]) {
            self.profile.controller.reset_reply_us
        } else {
            self.profile.controller.reply_us
        };
        let due_us = now + u64::from(reply_us);
        let mut out = AirOut {
            events: events.into_iter().map(|p| (due_us, p)).collect(),
            timers: Vec::new(),
        };
        let BleState {
            air,
            controller,
            central,
            ..
        } = st;
        air.sync(
            controller,
            central,
            now,
            &mut |b: &mut [u8]| g.draw_entropy(RngStream::RADIO_BLE, b),
            &mut out,
        );
        self.apply_air(st, g, out);
    }

    fn apply_air(&self, st: &mut BleState, g: &mut dyn GuestView, out: AirOut) {
        let mut instants: Vec<u64> = Vec::new();
        for (due_us, packet) in out.events {
            if !instants.contains(&due_us) {
                instants.push(due_us);
            }
            st.outbox.push(Outgoing { due_us, packet });
        }
        for due_us in instants {
            g.schedule(
                VTime::from_us(due_us),
                module_timer(self.module, TIMER_CONTROLLER),
            );
        }
        for (at_us, tag) in out.timers {
            g.schedule(VTime::from_us(at_us), module_timer(self.module, tag));
        }
    }
}

impl BleState {
    pub fn public_address(&self) -> [u8; 6] {
        let mut address = self.bt_mac;
        address.reverse();
        address
    }
}

impl BleHost {
    fn init(
        &self,
        st: &mut BleState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let (a0, scratch) = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.status(g) {
                    Ok(STATUS_IDLE) => {}
                    Ok(_) => return ret(ESP_ERR_INVALID_STATE),
                    Err(fail) => return fail,
                }
                let cfg = g.reg(pemu_hle::guest_call::A0);
                if cfg == 0 {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                let mut raw = [0u8; 14];
                if g.read(cfg, &mut raw).is_err() {
                    return hle_common::fault("ble", "esp_bt_controller_config_t", cfg);
                }
                let at = |off: u32| raw[off as usize];
                let stack = u32::from(u16::from_le_bytes([
                    at(cfg_off::STACK),
                    at(cfg_off::STACK + 1),
                ]));
                let prio = u32::from(at(cfg_off::PRIO));
                if prio != self.profile.worker.priority || stack < self.profile.worker.min_stack {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                if u32::from(at(cfg_off::MODE)) != MODE_BLE {
                    return ret(ESP_ERR_NOT_SUPPORTED);
                }
                let max_act = at(cfg_off::MAX_ACT);
                if max_act == 0 || max_act > MAX_ACT_LIMIT {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                if g.write(cfg + cfg_off::MAGIC, &CONFIG_MAGIC.to_le_bytes())
                    .is_err()
                {
                    return hle_common::fault("ble", "esp_bt_controller_config_t", cfg);
                }
                words[w::CFG] = cfg;
                words[w::STACK] = stack;
                words[w::PRIO] = prio;
                words[w::CPU] = u32::from(at(cfg_off::CPU));
                // On silicon the first `BLE_INIT` line is printed at the start of the call and the
                // controller's setup follows.
                self.read_mac(words)
            }
            step::SEMAPHORE => {
                if a0 == 0 {
                    return ret(ESP_ERR_NO_MEM);
                }
                st.semaphore = a0;
                let mut profile = bt_controller_profile();
                profile.stack_bytes = words[w::STACK];
                profile.priority = words[w::PRIO];
                profile.core_id = words[w::CPU];
                words[w::STEP] = step::TASK;
                HleAction::from(self.calls().create_worker(&self.pcs, &profile, a0))
            }
            step::TASK => {
                // pdPASS is 1; the handle is the `&handle` out-parameter at scratch offset 0.
                if a0 != 1 {
                    return ret(ESP_ERR_NO_MEM);
                }
                st.worker = word_at(scratch, 0);
                // The device maps source 8 to a CPU line at init whatever wakes the worker, so both
                // wake modes allocate it. In U5 nothing raises the source, but the guest still
                // holds the allocation.
                words[w::STEP] = step::INTR;
                HleAction::from(
                    CallRequest::new(
                        "esp_intr_alloc",
                        self.addr("esp_intr_alloc"),
                        &[
                            Arg::Val(u32::from(self.profile.worker.isr_source)),
                            Arg::Val(INTR_FLAG_LEVEL3),
                            Arg::Val(self.pcs.pc_of(MagicKind::BtIsr)),
                            Arg::Val(0),
                            Arg::Scratch(0),
                        ],
                    )
                    .with_scratch(vec![0; 4]),
                )
            }
            step::INTR => {
                if a0 != ESP_OK {
                    return ret(a0);
                }
                st.intr_handle = word_at(scratch, 0);
                words[w::LEDGER] = 0;
                self.ledger_alloc(Stage::Init, st, words, g)
            }
            step::MAC => {
                words[w::MAC_LO] = word_at(scratch, 0);
                words[w::MAC_HI] = word_at(scratch, 4);
                if let Some(mac) = scratch.get(..6) {
                    st.bt_mac.copy_from_slice(mac);
                }
                // A new init powers a controller in its default state (Core Vol 4 Part E §7.3.2).
                st.controller = Controller::default();
                st.outbox.clear();
                words[w::LINE] = 0;
                self.lines_or(Stage::Init, st, words, g)
            }
            step::LEDGER_ALLOC => self.ledger_allocated(Stage::Init, st, words, g, a0),
            step::LEDGER_FREE => self.free_or(st, words, Lifetime::Init, ESP_ERR_NO_MEM),
            step::INIT_DELAY => self.init_finish(words, g),
            step::TIMESTAMP | step::LOGGED => self.log_step(Stage::Init, st, words, g, a0),
            other => bad_step("ble.init", other),
        }
    }

    /// The capture-derived rows always; the rows one sdkconfig fixes only for an image of that
    /// shape.
    pub fn heap_plan(&self) -> Vec<&'static LedgerRow> {
        self.profile.heap_plan(self.lines_verified)
    }

    /// Init takes its `init` rows and each `boot` row not yet held; enable takes its `enable` rows.
    fn next_ledger_row(&self, st: &BleState, from: u32, stage: Stage) -> Option<u32> {
        let plan = self.heap_plan();
        (from as usize..plan.len())
            .find(|&i| {
                let index = u16::try_from(i).unwrap_or(u16::MAX);
                match (stage, plan[i].lifetime) {
                    (Stage::Init, Lifetime::Init) | (Stage::Enable, Lifetime::Enable) => true,
                    (Stage::Init, Lifetime::Boot) => !st.ledger.holds(index),
                    _ => false,
                }
            })
            .map(|i| i as u32)
    }

    /// One `heap_caps_malloc` per applicable `[[heap]]` row, so the guest heap carries what the
    /// replaced blob holds; goes on to the stage's lines when none is left.
    fn ledger_alloc(
        &self,
        stage: Stage,
        st: &mut BleState,
        words: &mut [u32],
        g: &mut dyn GuestView,
    ) -> HleAction {
        let plan = self.heap_plan();
        match self.next_ledger_row(st, words[w::LEDGER], stage) {
            Some(index) => {
                words[w::LEDGER] = index;
                words[w::STEP] = step::LEDGER_ALLOC;
                HleAction::from(CallRequest::new(
                    "heap_caps_malloc",
                    self.addr("heap_caps_malloc"),
                    &[
                        Arg::Val(plan[index as usize].bytes()),
                        Arg::Val(LEDGER_CAPS),
                    ],
                ))
            }
            None if stage == Stage::Init => self.init_finish(words, g),
            None => self.lines_or(stage, st, words, g),
        }
    }

    fn init_finish(&self, words: &mut [u32], g: &mut dyn GuestView) -> HleAction {
        if words[w::STEP] != step::INIT_DELAY
            && let Some(call) = self.busy(words, self.delays.init_us, step::INIT_DELAY)
        {
            return call;
        }
        match self.set_status(g, STATUS_INITED) {
            Ok(()) => ret(ESP_OK),
            Err(fail) => fail,
        }
    }

    /// A refused ledger allocation gives back what the stage took and reports no memory, as the
    /// device fails its controller call when the heap cannot hold it.
    fn ledger_allocated(
        &self,
        stage: Stage,
        st: &mut BleState,
        words: &mut [u32],
        g: &mut dyn GuestView,
        a0: u32,
    ) -> HleAction {
        let index = words[w::LEDGER];
        let plan = self.heap_plan();
        let Some(row) = plan.get(index as usize) else {
            return bad_step("ble ledger", step::LEDGER_ALLOC);
        };
        let lifetime = if stage == Stage::Enable {
            Lifetime::Enable
        } else {
            Lifetime::Init
        };
        if a0 == 0 {
            st.ledger.refused += 1;
            return self.free_or(st, words, lifetime, ESP_ERR_NO_MEM);
        }
        st.ledger
            .record(u16::try_from(index).unwrap_or(u16::MAX), a0, row.bytes());
        words[w::LEDGER] = index + 1;
        self.ledger_alloc(stage, st, words, g)
    }

    fn free_or(
        &self,
        st: &mut BleState,
        words: &mut [u32],
        lifetime: Lifetime,
        done: u32,
    ) -> HleAction {
        self.free_newest(st, words, lifetime)
            .unwrap_or_else(|| ret(done))
    }

    fn free_newest(
        &self,
        st: &mut BleState,
        words: &mut [u32],
        lifetime: Lifetime,
    ) -> Option<HleAction> {
        let block = st.ledger.take_newest(&self.heap_plan(), lifetime)?;
        words[w::STEP] = step::LEDGER_FREE;
        Some(HleAction::from(CallRequest::new(
            "heap_caps_free",
            self.addr("heap_caps_free"),
            &[Arg::Val(block.addr)],
        )))
    }

    fn read_mac(&self, words: &mut [u32]) -> HleAction {
        words[w::STEP] = step::MAC;
        HleAction::from(
            CallRequest::new(
                "esp_read_mac",
                self.addr("esp_read_mac"),
                &[Arg::Scratch(0), Arg::Val(ESP_MAC_BT)],
            )
            .with_scratch(vec![0; 8]),
        )
    }

    /// `None` when there is no busy time or no ROM busy-wait to spend it in.
    fn busy(&self, words: &mut [u32], us: u32, next: u32) -> Option<HleAction> {
        if us == 0 || self.delay_fn == 0 {
            return None;
        }
        words[w::STEP] = next;
        Some(HleAction::from(CallRequest::new(
            "ets_delay_us",
            self.delay_fn,
            &[Arg::Val(us)],
        )))
    }

    fn lines_or(
        &self,
        stage: Stage,
        st: &mut BleState,
        words: &mut [u32],
        g: &mut dyn GuestView,
    ) -> HleAction {
        if let Some(call) = self.next_line(stage, words) {
            return call;
        }
        match stage {
            Stage::Init => {
                words[w::STEP] = step::SEMAPHORE;
                HleAction::from(self.calls().create_semaphore())
            }
            Stage::Enable => {
                if words[w::STEP] != step::ENABLE_DELAY
                    && let Some(call) = self.busy(words, self.enable_delay_us(), step::ENABLE_DELAY)
                {
                    return call;
                }
                st.phy_logged = true;
                match self.set_status(g, STATUS_ENABLED) {
                    Ok(()) => ret(ESP_OK),
                    Err(fail) => fail,
                }
            }
            // The other stages are the Wi-Fi module's.
            Stage::Start | Stage::Stop | Stage::DeinitRefused => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                "the ble module has only the init and enable stages",
            )),
        }
    }

    fn log_step(
        &self,
        stage: Stage,
        st: &mut BleState,
        words: &mut [u32],
        g: &mut dyn GuestView,
        a0: u32,
    ) -> HleAction {
        if words[w::STEP] == step::TIMESTAMP {
            let Some((tag, text)) = self.line_text(stage, words) else {
                return bad_step("ble log line", words[w::LINE]);
            };
            words[w::STEP] = step::LOGGED;
            return self.log_call(&tag, &text, a0);
        }
        // Counted once `esp_log` returned, so a line whose call faulted is not reported.
        st.synthesized_lines += 1;
        words[w::LINE] += 1;
        self.lines_or(stage, st, words, g)
    }

    fn enable(
        &self,
        st: &mut BleState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let (a0, _) = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.status(g) {
                    Ok(STATUS_INITED) => {}
                    Ok(_) => return ret(ESP_ERR_INVALID_STATE),
                    Err(fail) => return fail,
                }
                if g.reg(pemu_hle::guest_call::A0) != MODE_BLE {
                    return ret(ESP_ERR_INVALID_ARG);
                }
                if st.phy_logged {
                    words[w::LINE] = u32::MAX;
                }
                words[w::LEDGER] = 0;
                self.ledger_alloc(Stage::Enable, st, words, g)
            }
            step::LEDGER_ALLOC => self.ledger_allocated(Stage::Enable, st, words, g, a0),
            step::LEDGER_FREE => self.free_or(st, words, Lifetime::Enable, ESP_ERR_NO_MEM),
            step::ENABLE_DELAY => self.lines_or(Stage::Enable, st, words, g),
            step::TIMESTAMP | step::LOGGED => self.log_step(Stage::Enable, st, words, g, a0),
            other => bad_step("ble.enable", other),
        }
    }

    fn disable(&self, st: &mut BleState, words: &mut Vec<u32>, g: &mut dyn GuestView) -> HleAction {
        words.resize(w::LEN, 0);
        match words[w::STEP] {
            step::ENTRY => {
                match self.status(g) {
                    Ok(STATUS_ENABLED) => {}
                    Ok(_) => return ret(ESP_ERR_INVALID_STATE),
                    Err(fail) => return fail,
                }
                self.disable_rest(st, words, g)
            }
            step::LEDGER_FREE | step::DISABLE_DELAY => self.disable_rest(st, words, g),
            other => bad_step("ble.disable", other),
        }
    }

    fn disable_rest(
        &self,
        st: &mut BleState,
        words: &mut [u32],
        g: &mut dyn GuestView,
    ) -> HleAction {
        if words[w::STEP] != step::DISABLE_DELAY {
            if let Some(call) = self.free_newest(st, words, Lifetime::Enable) {
                return call;
            }
            if let Some(call) = self.busy(words, self.delays.disable_us, step::DISABLE_DELAY) {
                return call;
            }
        }
        match self.set_status(g, STATUS_INITED) {
            Ok(()) => ret(ESP_OK),
            Err(fail) => fail,
        }
    }

    fn delete_worker(&self, st: &BleState, words: &mut [u32]) -> HleAction {
        words[w::STEP] = step::DELETE_TASK;
        HleAction::from(CallRequest::new(
            "vTaskDelete",
            self.addr("vTaskDelete"),
            &[Arg::Val(st.worker)],
        ))
    }

    fn deinit(
        &self,
        st: &mut BleState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
        resume: &Resume,
    ) -> HleAction {
        words.resize(w::LEN, 0);
        let (a0, _) = returned(resume);
        match words[w::STEP] {
            step::ENTRY => {
                match self.status(g) {
                    Ok(STATUS_INITED) => {}
                    Ok(_) => return ret(ESP_ERR_INVALID_STATE),
                    Err(fail) => return fail,
                }
                // A boot block stays: the device's free heap after deinit stays below its value
                // before init.
                self.free_newest(st, words, Lifetime::Init)
                    .unwrap_or_else(|| self.after_ledger_freed(st, words))
            }
            step::LEDGER_FREE => self
                .free_newest(st, words, Lifetime::Init)
                .unwrap_or_else(|| self.after_ledger_freed(st, words)),
            step::INTR_FREED => {
                if a0 != ESP_OK {
                    return ret(a0);
                }
                st.intr_handle = 0;
                self.delete_worker(st, words)
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
                st.worker = 0;
                st.semaphore = 0;
                st.pending_rx.clear();
                st.outbox.clear();
                if let Some(call) = self.busy(words, self.delays.deinit_us, step::DEINIT_DELAY) {
                    return call;
                }
                match self.set_status(g, STATUS_IDLE) {
                    Ok(()) => ret(ESP_OK),
                    Err(fail) => fail,
                }
            }
            step::DEINIT_DELAY => match self.set_status(g, STATUS_IDLE) {
                Ok(()) => ret(ESP_OK),
                Err(fail) => fail,
            },
            other => bad_step("ble.deinit", other),
        }
    }

    fn after_ledger_freed(&self, st: &mut BleState, words: &mut [u32]) -> HleAction {
        if st.intr_handle != 0 {
            words[w::STEP] = step::INTR_FREED;
            return HleAction::from(CallRequest::new(
                "esp_intr_free",
                self.addr("esp_intr_free"),
                &[Arg::Val(st.intr_handle)],
            ));
        }
        self.delete_worker(st, words)
    }

    fn immediate(&self, kind: u16, st: &mut BleState, g: &mut dyn GuestView) -> HleAction {
        let status = match self.status(g) {
            Ok(status) => status,
            Err(fail) => return fail,
        };
        match kind {
            handler::CHECK_SEND => ret(u32::from(status == STATUS_ENABLED)),
            handler::REGISTER_CB if status != STATUS_ENABLED => ret(ESP_FAIL),
            _ => {
                let cb = g.reg(pemu_hle::guest_call::A0);
                let mut raw = [0u8; 8];
                if g.read(cb, &mut raw).is_err() {
                    return hle_common::fault("ble", "esp_vhci_host_callback_t", cb);
                }
                st.cb_send_available = word_at(&raw, 0);
                st.cb_recv = word_at(&raw, 4);
                ret(ESP_OK)
            }
        }
    }

    fn send_packet(
        &self,
        st: &mut BleState,
        words: &mut Vec<u32>,
        g: &mut dyn GuestView,
    ) -> HleAction {
        words.resize(1, 0);
        match words[0] {
            step::ENTRY => {
                match self.status(g) {
                    Ok(STATUS_ENABLED) => {}
                    Ok(_) => return ret(0),
                    Err(fail) => return fail,
                }
                let data = g.reg(pemu_hle::guest_call::A0);
                let len = g.reg(pemu_hle::guest_call::A0 + 1) & 0xFFFF;
                if len > MAX_H4 {
                    // Refused whole, never truncated into a different packet, and the sender is not
                    // re-armed.
                    st.refused_tx += 1;
                    return ret(0);
                }
                let mut packet = vec![0u8; len as usize];
                if g.read(data, &mut packet).is_err() {
                    return hle_common::fault("ble", "the H4 packet", data);
                }
                st.tx_packets += 1;
                st.recent_tx.push(packet.iter().copied().take(4).collect());
                if st.recent_tx.len() > RECENT_TX {
                    st.recent_tx.remove(0);
                }
                // With the external bridge up the virtual controller never sees the packet. The
                // capture still records it: btsnoop is the VHCI boundary, whoever is on the other
                // side.
                if st.external.attached {
                    st.capture
                        .record(g.now().as_us(), btsnoop::dir::TO_CONTROLLER, &packet);
                    if st.external.outbound.len() >= EXTERNAL_OUT_MAX {
                        st.external.outbound.remove(0);
                        st.external.dropped_out += 1;
                    }
                    // Key material is zeroed where it is stored, not where it is written out, so no
                    // snapshot section ever holds a plain key.
                    btsnoop::redact(&mut packet);
                    st.external.outbound.push(packet);
                    st.external.sent += 1;
                } else {
                    self.to_controller(st, g, &packet);
                }
                if st.cb_send_available == 0 {
                    return ret(0);
                }
                words[0] = step::NOTIFIED;
                HleAction::from(CallRequest::new(
                    "notify_host_send_available",
                    st.cb_send_available,
                    &[],
                ))
            }
            step::NOTIFIED => ret(0),
            other => bad_step("ble.send_packet", other),
        }
    }

    /// Parks, and after each wake delivers every pending event to `notify_host_recv` in task
    /// context.
    fn worker(
        &self,
        st: &mut BleState,
        words: &mut Vec<u32>,
        resume: &Resume,
        now_us: u64,
    ) -> HleAction {
        words.resize(1, 0);
        if matches!(resume, Resume::Returned { .. }) && words[0] == step::DELIVERED {
            st.rx_packets += 1;
        }
        if let Resume::Woken {
            reason: WakeReason::Deinit,
        } = resume
        {
            // The deinit handler deletes this task from the caller; until then the worker waits.
            words[0] = step::PARKED;
            return HleAction::Park;
        }
        if st.cb_recv == 0 && !st.pending_rx.is_empty() {
            st.dropped_rx += st.pending_rx.len() as u64;
            st.pending_rx.clear();
        }
        if st.pending_rx.is_empty() {
            words[0] = step::PARKED;
            return HleAction::Park;
        }
        let packet = st.pending_rx.remove(0);
        st.capture.record(now_us, btsnoop::dir::TO_HOST, &packet);
        let len = packet.len() as u32;
        let mut scratch = packet;
        scratch.resize(
            scratch
                .len()
                .max(pemu_hle::guest_call::SCRATCH_WORD)
                .next_multiple_of(4),
            0,
        );
        words[0] = step::DELIVERED;
        HleAction::from(
            CallRequest::new(
                "notify_host_recv",
                st.cb_recv,
                &[Arg::Scratch(0), Arg::Val(len)],
            )
            .with_scratch(scratch),
        )
    }

    fn step(
        &self,
        name: &str,
        state_bytes: &mut Vec<u8>,
        handler: &mut HandlerState,
        g: &mut dyn GuestView,
        resume: Resume,
    ) -> HleAction {
        let mut st = match BleState::decode(state_bytes) {
            Ok(st) => st,
            Err(err) => return HleAction::Fail(err),
        };
        let mut words = words_of(handler);
        let out = match name {
            "ble.init" => self.init(&mut st, &mut words, g, &resume),
            "ble.enable" => self.enable(&mut st, &mut words, g, &resume),
            "ble.deinit" => self.deinit(&mut st, &mut words, g, &resume),
            "ble.send_packet" => self.send_packet(&mut st, &mut words, g),
            "ble.worker" => self.worker(&mut st, &mut words, &resume, g.now().as_us()),
            "ble.disable" => self.disable(&mut st, &mut words, g),
            "ble.check_send_available" => self.immediate(handler::CHECK_SEND, &mut st, g),
            "ble.register_callback" => self.immediate(handler::REGISTER_CB, &mut st, g),
            other => HleAction::Fail(HleError::new(
                HleErrorKind::Handler,
                format!("the ble module has no handler `{other}`"),
            )),
        };
        store_words(handler, &words);
        *state_bytes = st.encode();
        out
    }
}

impl ModuleHost for BleHost {
    fn module(&self) -> ModuleIndex {
        self.module
    }

    fn name(&self) -> &'static str {
        "ble"
    }

    fn magic_entries(&self) -> Vec<MagicKind> {
        vec![MagicKind::BtWorker, MagicKind::BtIsr]
    }

    fn set_busy_delays(&mut self, delays: BusyDelays) {
        self.delays = delays;
    }

    fn workers(&mut self, wake: WakeMode) -> Vec<WorkerConfig> {
        let mut profile = bt_controller_profile();
        profile.poll_ticks = self.profile.worker.poll_ticks;
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
                    format!("the ble module has no handler {}", kind.0),
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
        const NAMES: [&str; 11] = [
            "xTaskCreatePinnedToCore",
            "xQueueGenericCreate",
            "xQueueSemaphoreTake",
            "xQueueGiveFromISR",
            "vPortYieldFromISR",
            "vTaskDelete",
            "vQueueDelete",
            "esp_log",
            "esp_log_timestamp",
            "esp_read_mac",
            "esp_intr_alloc",
        ];
        let name = NAMES.into_iter().find(|n| self.addr(n) == func)?;
        Some(CallInfo {
            name,
            // Only the FromISR calls are legal inside an ISR.
            blocking: !name.ends_with("FromISR"),
            _func: func,
        })
    }

    fn log_lines(&self, state: &[u8]) -> Option<pemu_hle::binding::RadioLogLines> {
        let synthesized = if state.is_empty() {
            0
        } else {
            BleState::decode(state).ok()?.synthesized_lines
        };
        Some(pemu_hle::binding::RadioLogLines {
            synthesized: u64::from(synthesized),
            verified: self.lines_verified,
        })
    }

    /// One bridge at a time, so 0 or 1.
    fn bridges_live(&self, state: &[u8]) -> u32 {
        if state.is_empty() {
            return 0;
        }
        BleState::decode(state)
            .map(|st| u32::from(st.external.attached))
            .unwrap_or(0)
    }

    fn heap_ledger(&self, state: &[u8]) -> Vec<pemu_hle::binding::HeapBlock> {
        if state.is_empty() {
            return Vec::new();
        }
        let Ok(st) = BleState::decode(state) else {
            return Vec::new();
        };
        st.ledger.report("ble", &self.heap_plan())
    }

    fn on_timer(
        &mut self,
        state: &mut Vec<u8>,
        tag: u16,
        g: &mut dyn GuestView,
    ) -> Vec<RadioEvent> {
        let Ok(mut st) = BleState::decode(state) else {
            return Vec::new();
        };
        let now = g.now().as_us();
        if tag != TIMER_CONTROLLER {
            let public = st.public_address();
            let BleState {
                air,
                controller,
                central,
                ..
            } = &mut st;
            let mut entropy = |b: &mut [u8]| g.draw_entropy(RngStream::RADIO_BLE, b);
            let out = match tag {
                air::TIMER_ADVERTISING => {
                    air.on_advertising_timer(controller, central, public, now, &mut entropy)
                }
                air::TIMER_CONNECTION => {
                    air.on_connection_timer(controller, central, now, &mut entropy)
                }
                air::TIMER_CENTRAL => air.on_central_timer(controller, central, now, &mut entropy),
                _ => return Vec::new(),
            };
            // Host events are released by the controller timers `apply_air` scheduled, so an air
            // timer posts nothing itself.
            self.apply_air(&mut st, g, out);
            *state = st.encode();
            return Vec::new();
        }
        let (due, later): (Vec<Outgoing>, Vec<Outgoing>) =
            st.outbox.drain(..).partition(|o| o.due_us <= now);
        st.outbox = later;
        *state = st.encode();
        due.into_iter()
            .map(|o| RadioEvent {
                tag: TIMER_CONTROLLER,
                payload: o.packet,
            })
            .collect()
    }

    fn on_input(
        &mut self,
        state: &mut Vec<u8>,
        payload: &[u8],
        g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        let steps =
            central::decode_script(payload).map_err(|e| HleError::new(HleErrorKind::Handler, e))?;
        let mut st = BleState::decode(state)?;
        let now = g.now().as_us();
        // A `capture` step is the module's: applied when the script is journaled, with no step
        // result.
        let (capture, steps): (Vec<central::Step>, Vec<central::Step>) =
            steps.into_iter().partition(|s| {
                matches!(
                    s,
                    central::Step::Capture { .. } | central::Step::CaptureSecrets { .. }
                )
            });
        for step in capture {
            match step {
                central::Step::Capture { enabled } => st.capture.set_enabled(enabled),
                central::Step::CaptureSecrets { secrets } => st.capture.set_secrets(secrets),
                _ => {}
            }
        }
        st.central.push_steps(steps, now);
        let mut out = AirOut::default();
        let BleState {
            air,
            controller,
            central,
            ..
        } = &mut st;
        air.sync(
            controller,
            central,
            now,
            &mut |b: &mut [u8]| g.draw_entropy(RngStream::RADIO_BLE, b),
            &mut out,
        );
        self.apply_air(&mut st, g, out);
        *state = st.encode();
        Ok(Vec::new())
    }

    /// The one door for a host-posted radio event. Every packet was journaled with its origin, so a
    /// run that used the bridge replays to the same state. `Attach` and `Detach` change only where
    /// the guest's packets go.
    fn on_hci(
        &mut self,
        state: &mut Vec<u8>,
        ev: HciInput<'_>,
        g: &mut dyn GuestView,
    ) -> Result<Vec<RadioEvent>, HleError> {
        let mut st = BleState::decode(state)?;
        let events = match ev {
            HciInput::Attach => {
                // A fresh session: numbering restarts and nothing an earlier peer left is handed
                // over.
                st.external = External {
                    attached: true,
                    ..External::default()
                };
                Vec::new()
            }
            HciInput::Detach => {
                st.external.attached = false;
                st.external.outbound.clear();
                Vec::new()
            }
            HciInput::Packet { seq, data } => {
                if data.is_empty() || data.len() > MAX_H4 as usize {
                    // Refused whole rather than truncated; the machine counts the input unapplied.
                    return Err(HleError::new(
                        HleErrorKind::Handler,
                        format!("an external HCI packet of {} bytes", data.len()),
                    ));
                }
                // A gap is data the transport lost, which the journal must make visible.
                if seq > st.external.next_seq {
                    st.external.lost_in += seq - st.external.next_seq;
                }
                st.external.next_seq = st.external.next_seq.max(seq.saturating_add(1));
                st.external.inbound += 1;
                // Delivered like the virtual controller's answers, through the worker.
                vec![RadioEvent {
                    tag: 0,
                    payload: data.to_vec(),
                }]
            }
        };
        let _ = g;
        *state = st.encode();
        Ok(events)
    }

    fn deliver(&mut self, state: &mut Vec<u8>, _entry: MagicKind, events: Vec<RadioEvent>) {
        if let Ok(mut st) = BleState::decode(state) {
            for event in events {
                // Too long for the worker's scratch: counted as dropped rather than failing the
                // nested call.
                if event.payload.len() > MAX_H4 as usize {
                    st.dropped_rx += 1;
                } else {
                    st.pending_rx.push(event.payload);
                }
            }
            *state = st.encode();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::sched::{EventHandle, EventKey};
    use pemu_core::time::VTime;
    use pemu_loader::elf::ElfInfo;
    use pemu_loader::symbols::{SymBind, SymKind, SymSection, Symbol, SymbolTable};
    use pemu_rv32::trap::Trap;

    const STATUS: u32 = 0x3FC9_0000;
    const CFG: u32 = 0x3FC9_1000;

    /// Registers and sparse memory; the test answers the nested calls.
    #[derive(Default)]
    struct Guest {
        x: [u32; 32],
        mem: BTreeMap<u32, u8>,
        now: VTime,
        sched: pemu_core::sched::Scheduler,
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
            self.sched.schedule(self.now, at, key)
        }
        fn now(&self) -> VTime {
            self.now
        }
        fn symbol(&self, _name: &str) -> Option<u32> {
            None
        }
        fn current_task(&mut self) -> u32 {
            0x3FCA_0000
        }
        fn in_isr(&mut self) -> bool {
            false
        }
        fn scheduler_running(&mut self) -> bool {
            true
        }
    }

    fn host() -> BleHost {
        let profile = BleProfile::load();
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
        syms.push(Symbol {
            name: "btdm_controller_status".to_string(),
            addr: STATUS,
            size: 4,
            kind: SymKind::Object,
            bind: SymBind::Local,
            section: SymSection::Index(2),
        });
        let elf = ElfInfo {
            sha256: [0; 32],
            entry: 0,
            sections: Vec::new(),
            segments: Vec::new(),
            symbols: SymbolTable::new(syms),
            app_desc: None,
        };
        BleHost::new(ModuleIndex::FIRST_MODULE, &ImageView::symbols_only(&elf)).expect("host")
    }

    fn config(g: &mut Guest, prio: u8, stack: u16, mode: u8, max_act: u8) {
        let mut raw = [0u8; 16];
        raw[8..10].copy_from_slice(&stack.to_le_bytes());
        raw[10] = prio;
        raw[12] = mode;
        raw[13] = max_act;
        g.write(CFG, &raw).unwrap();
        g.x[10] = CFG;
    }

    fn called(action: &HleAction) -> (u32, Vec<u8>) {
        match action {
            HleAction::Call { func, scratch, .. } => (*func, scratch.clone()),
            other => panic!("expected a nested call, got {other:?}"),
        }
    }

    fn back(a0: u32, scratch: Vec<u8>) -> Resume {
        Resume::Returned { a0, a1: 0, scratch }
    }

    const LEDGER_BASE: u32 = 0x3FCE_0000;

    fn init_rows(h: &BleHost) -> Vec<&'static LedgerRow> {
        h.heap_plan()
            .into_iter()
            .filter(|r| r.lifetime != Lifetime::Enable)
            .collect()
    }

    /// Answers each ledger `heap_caps_malloc` with the n-th block at `base + n * 0x1000`, checking
    /// the size of the next of `rows` and the internal, byte-addressable capabilities.
    fn answer_ledger(
        h: &mut BleHost,
        st: &mut Vec<u8>,
        hs: &mut HandlerState,
        g: &mut Guest,
        mut a: HleAction,
        rows: &[&LedgerRow],
        base: u32,
    ) -> HleAction {
        let malloc = h.addr("heap_caps_malloc");
        let mut n = 0usize;
        while let HleAction::Call { func, args, .. } = &a {
            if *func != malloc {
                break;
            }
            assert!(n < rows.len(), "more blocks asked for than planned");
            assert_eq!(
                (args[0], args[1]),
                (Arg::Val(rows[n].bytes()), Arg::Val(LEDGER_CAPS)),
                "ledger row {}",
                rows[n].label
            );
            a = h.resume(st, hs, g, back(base + (n as u32) * 0x1000, vec![]));
            n += 1;
        }
        assert_eq!(n, rows.len(), "every planned block was asked for");
        a
    }

    fn answer_frees(
        h: &mut BleHost,
        st: &mut Vec<u8>,
        hs: &mut HandlerState,
        g: &mut Guest,
        mut a: HleAction,
    ) -> (HleAction, Vec<Arg>) {
        let free = h.addr("heap_caps_free");
        let mut freed = Vec::new();
        while let HleAction::Call { func, args, .. } = &a {
            if *func != free {
                break;
            }
            freed.push(args[0]);
            a = h.resume(st, hs, g, back(0, vec![]));
        }
        (a, freed)
    }

    /// Newest first: the order a deinit gives them back in.
    fn newest_first(n: usize) -> Vec<Arg> {
        (0..n)
            .rev()
            .map(|i| Arg::Val(LEDGER_BASE + (i as u32) * 0x1000))
            .collect()
    }

    fn answer_lines(
        h: &mut BleHost,
        st: &mut Vec<u8>,
        hs: &mut HandlerState,
        g: &mut Guest,
        mut a: HleAction,
    ) -> (HleAction, Vec<String>) {
        let mut printed = Vec::new();
        while let HleAction::Call { func, scratch, .. } = &a {
            let func = *func;
            if func == h.addr("esp_log_timestamp") {
                a = h.resume(st, hs, g, back(407, vec![]));
            } else if func == h.addr("esp_log") {
                let text = scratch.split(|b| *b == 0).nth(2).expect("text").to_vec();
                printed.push(String::from_utf8(text).expect("utf-8"));
                a = h.resume(st, hs, g, back(40, vec![]));
            } else {
                break;
            }
        }
        (a, printed)
    }

    /// Answers the setup after an init's lines: semaphore, task and the radio interrupt, allocated
    /// on source 8 at level 3 in both wake modes.
    fn answer_setup(
        h: &mut BleHost,
        st: &mut Vec<u8>,
        hs: &mut HandlerState,
        g: &mut Guest,
        a: HleAction,
        (sem, task, intr): (u32, u32, u32),
    ) -> HleAction {
        assert_eq!(called(&a).0, h.addr("xQueueGenericCreate"));
        let a = h.resume(st, hs, g, back(sem, vec![]));
        let (func, scratch) = called(&a);
        assert_eq!(func, h.addr("xTaskCreatePinnedToCore"));
        assert!(scratch.windows(12).any(|w| w == b"btController"));
        let mut handle = task.to_le_bytes().to_vec();
        handle.extend_from_slice(&scratch[4..]);
        let a = h.resume(st, hs, g, back(1, handle));
        let HleAction::Call { func, args, .. } = &a else {
            panic!("{a:?}")
        };
        assert_eq!(*func, h.addr("esp_intr_alloc"));
        assert_eq!(
            (args[0], args[1]),
            (
                Arg::Val(u32::from(h.profile.worker.isr_source)),
                Arg::Val(INTR_FLAG_LEVEL3)
            )
        );
        h.resume(st, hs, g, back(ESP_OK, intr.to_le_bytes().to_vec()))
    }

    #[test]
    fn init_validates_prints_the_lines_creates_the_worker_and_marks_inited() {
        let mut h = host();
        // The synthetic image has no code bytes to match, so set what binding found for `pk`.
        h.lines_verified = true;
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        config(&mut g, 23, 4096, 1, 6);
        let mut st = Vec::new();
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(hs.handler, "ble.init");
        let mut buf = [0u8; 4];
        g.read(CFG, &mut buf).unwrap();
        assert_eq!(u32::from_le_bytes(buf), CONFIG_MAGIC);
        assert_eq!(called(&a).0, h.addr("esp_read_mac"));
        let mac = vec![0x02, 0x00, 0x00, 0x12, 0x34, 0x56, 0, 0];
        let mut a = h.resume(&mut st, &mut hs, &mut g, back(0, mac));
        let mut printed = Vec::new();
        for _ in 0..4 {
            assert_eq!(called(&a).0, h.addr("esp_log_timestamp"));
            a = h.resume(&mut st, &mut hs, &mut g, back(407, vec![]));
            let (func, scratch) = called(&a);
            assert_eq!(func, h.addr("esp_log"));
            let HleAction::Call { args, nargs, .. } = &a else {
                unreachable!()
            };
            assert_eq!((*nargs, args[0], args[3]), (6, Arg::Val(3), Arg::Val(407)));
            let strings: Vec<String> = scratch
                .split(|b| *b == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect();
            assert_eq!(strings[0], "BLE_INIT");
            assert_eq!(strings[1], "I (%lu) %s: %s\n");
            printed.push(strings[2].clone());
            a = h.resume(&mut st, &mut hs, &mut g, back(40, vec![]));
        }
        assert_eq!(printed[0], "BT controller compile version [1bb2f50]");
        assert_eq!(printed[3], "Bluetooth MAC: 02:00:00:12:34:56");
        let a = answer_setup(
            &mut h,
            &mut st,
            &mut hs,
            &mut g,
            a,
            (0x3FCB_0000, 0x3FCC_0000, 0x3FCD_0000),
        );
        let rows = init_rows(&h);
        let a = answer_ledger(&mut h, &mut st, &mut hs, &mut g, a, &rows, LEDGER_BASE);
        assert_eq!(a, ret(ESP_OK));
        g.read(STATUS, &mut buf).unwrap();
        assert_eq!(u32::from_le_bytes(buf), STATUS_INITED);
        let state = BleState::decode(&st).unwrap();
        assert_eq!(
            (state.semaphore, state.worker, state.intr_handle),
            (0x3FCB_0000, 0x3FCC_0000, 0x3FCD_0000)
        );
        assert_eq!(state.synthesized_lines, 4);
        assert_eq!(rows.len(), h.heap_plan().len() - 1);
        assert_eq!(state.ledger.blocks.len(), rows.len());
        assert_eq!(state.ledger.refused, 0);
        assert_eq!(
            state.ledger.held(),
            rows.iter().map(|r| u64::from(r.bytes())).sum::<u64>()
        );

        let (_, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(a, ret(ESP_ERR_INVALID_STATE));
    }

    fn init_ok(h: &mut BleHost, st: &mut Vec<u8>, g: &mut Guest, sem: u32, task: u32, intr: u32) {
        config(g, 23, 4096, 1, 6);
        let (mut hs, a) = h.enter(st, HandlerKind(handler::INIT), g);
        assert_eq!(called(&a).0, h.addr("esp_read_mac"));
        let a = h.resume(st, &mut hs, g, back(0, vec![0x02, 0, 0, 1, 2, 3, 0, 0]));
        let (a, _) = answer_lines(h, st, &mut hs, g, a);
        let a = answer_setup(h, st, &mut hs, g, a, (sem, task, intr));
        let rows = init_rows(h);
        let a = answer_ledger(h, st, &mut hs, g, a, &rows, LEDGER_BASE);
        assert_eq!(a, ret(ESP_OK));
    }

    #[test]
    fn a_u4_deinit_frees_the_interrupt_drops_undelivered_packets_and_init_works_again() {
        let mut h = host();
        h.workers(WakeMode::U4MagicIsr);
        let mut g = Guest::default();
        let mut st = Vec::new();
        init_ok(
            &mut h,
            &mut st,
            &mut g,
            0x3FCB_0000,
            0x3FCC_0000,
            0x3FCD_0000,
        );
        assert_eq!(BleState::decode(&st).unwrap().intr_handle, 0x3FCD_0000);
        h.deliver(
            &mut st,
            MagicKind::BtWorker,
            vec![RadioEvent {
                tag: 0,
                payload: vec![0x04, 0x0E],
            }],
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        let (a, freed) = answer_frees(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(
            freed,
            newest_first(init_rows(&h).len()),
            "every block is given back, newest first"
        );
        assert!(
            BleState::decode(&st).unwrap().ledger.blocks.is_empty(),
            "the ledger holds nothing after a deinit"
        );
        let HleAction::Call { func, args, .. } = &a else {
            panic!("{a:?}")
        };
        assert_eq!(
            (*func, args[0]),
            (h.addr("esp_intr_free"), Arg::Val(0x3FCD_0000))
        );
        let a = h.resume(&mut st, &mut hs, &mut g, back(ESP_OK, vec![]));
        let HleAction::Call { func, args, .. } = &a else {
            panic!("{a:?}")
        };
        assert_eq!(
            (*func, args[0]),
            (h.addr("vTaskDelete"), Arg::Val(0x3FCC_0000))
        );
        let a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        assert_eq!(called(&a).0, h.addr("vQueueDelete"));
        let a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        assert_eq!(a, ret(ESP_OK));
        let state = BleState::decode(&st).unwrap();
        assert_eq!(
            (state.intr_handle, state.worker, state.semaphore),
            (0, 0, 0)
        );
        assert!(
            state.pending_rx.is_empty(),
            "undelivered packets are dropped"
        );
        let mut buf = [0u8; 4];
        g.read(STATUS, &mut buf).unwrap();
        assert_eq!(u32::from_le_bytes(buf), STATUS_IDLE);

        init_ok(
            &mut h,
            &mut st,
            &mut g,
            0x3FCB_1000,
            0x3FCC_1000,
            0x3FCD_1000,
        );
        let state = BleState::decode(&st).unwrap();
        assert_eq!(
            (state.intr_handle, state.worker, state.semaphore),
            (0x3FCD_1000, 0x3FCC_1000, 0x3FCB_1000)
        );
    }

    #[test]
    fn the_external_bridge_diverts_the_guests_packets_and_redacts_what_it_holds() {
        let mut h = host();
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        g.write(STATUS, &STATUS_ENABLED.to_le_bytes()).unwrap();
        let mut st = BleState::default().encode();
        h.on_hci(&mut st, HciInput::Attach, &mut g)
            .expect("the bridge attaches");
        assert!(BleState::decode(&st).unwrap().external.attached);
        assert_eq!(h.bridges_live(&st), 1);

        // `HCI_LE_Encrypt`: a 16-byte key and 16 bytes of plaintext, both secrets.
        let mut packet = vec![super::super::hci::H4_COMMAND, 0x17, 0x20, 32];
        packet.extend_from_slice(&[0xAB; 32]);
        let at = 0x3FC9_2000;
        g.write(at, &packet).unwrap();
        g.x[10] = at;
        g.x[11] = packet.len() as u32;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SEND_PACKET), &mut g);
        assert_eq!(a, ret(0), "no send-available callback is registered");
        let ext = BleState::decode(&st).unwrap().external;
        assert_eq!(ext.sent, 1);
        assert_eq!(ext.outbound.len(), 1);
        assert!(
            !ext.outbound[0][4..].contains(&0xAB),
            "the key and the plaintext must be zeroed where they are stored"
        );
        assert_eq!(
            ext.outbound[0][..4],
            packet[..4],
            "the header is kept, so the peer still sees the opcode"
        );
        assert!(BleState::decode(&st).unwrap().outbox.is_empty());

        let before = st.clone();
        let (packets, next) = ext.since(0);
        assert_eq!(packets.len(), 1);
        assert_eq!(next, 1);
        assert_eq!(ext.since(1).0.len(), 0, "nothing new since the last read");
        assert_eq!(
            ext.since(99).0.len(),
            0,
            "a cursor past the end reads nothing"
        );
        assert_eq!(st, before, "reading the window is not a change");

        h.on_hci(&mut st, HciInput::Detach, &mut g)
            .expect("the bridge detaches");
        let ext = BleState::decode(&st).unwrap().external;
        assert!(!ext.attached && ext.outbound.is_empty());
        assert_eq!(h.bridges_live(&st), 0);
    }

    #[test]
    fn an_inbound_external_packet_becomes_one_radio_event_and_a_gap_is_counted() {
        let mut h = host();
        let mut g = Guest::default();
        let mut st = BleState::default().encode();
        let events = h
            .on_hci(
                &mut st,
                HciInput::Packet {
                    seq: 0,
                    data: &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
                },
                &mut g,
            )
            .expect("a well-formed packet");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload.len(), 7);
        h.on_hci(
            &mut st,
            HciInput::Packet {
                seq: 3,
                data: &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
            },
            &mut g,
        )
        .expect("a well-formed packet");
        let ext = BleState::decode(&st).unwrap().external;
        assert_eq!((ext.inbound, ext.lost_in, ext.next_seq), (2, 2, 4));
        // A repeated or older packet does not move the expected number back, so the next in-order
        // one is not counted as lost twice.
        for seq in [3, 1, 4] {
            h.on_hci(
                &mut st,
                HciInput::Packet {
                    seq,
                    data: &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
                },
                &mut g,
            )
            .expect("a well-formed packet");
        }
        let ext = BleState::decode(&st).unwrap().external;
        assert_eq!((ext.inbound, ext.lost_in, ext.next_seq), (5, 2, 5));
        for data in [vec![], vec![0u8; MAX_H4 as usize + 1]] {
            assert!(
                h.on_hci(
                    &mut st,
                    HciInput::Packet {
                        seq: 4,
                        data: &data
                    },
                    &mut g
                )
                .is_err()
            );
        }
        assert_eq!(
            BleState::decode(&st).unwrap().external.inbound,
            5,
            "refused, not counted"
        );
    }

    #[test]
    fn an_init_whose_ledger_block_is_refused_reports_no_memory_and_gives_back_what_it_took() {
        // A device too tight for the controller fails its init, so the emulator must too.
        let mut h = host();
        h.lines_verified = true;
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        config(&mut g, 23, 4096, 1, 6);
        let mut st = Vec::new();
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(called(&a).0, h.addr("esp_read_mac"));
        let a = h.resume(
            &mut st,
            &mut hs,
            &mut g,
            back(0, vec![0x02, 0, 0, 1, 2, 3, 0, 0]),
        );
        let (a, _) = answer_lines(&mut h, &mut st, &mut hs, &mut g, a);
        let mut a = answer_setup(
            &mut h,
            &mut st,
            &mut hs,
            &mut g,
            a,
            (0x3FCB_0000, 0x3FCC_0000, 0x3FCD_0000),
        );
        assert_eq!(called(&a).0, h.addr("heap_caps_malloc"));
        a = h.resume(&mut st, &mut hs, &mut g, back(LEDGER_BASE, vec![]));
        assert_eq!(called(&a).0, h.addr("heap_caps_malloc"));
        a = h.resume(&mut st, &mut hs, &mut g, back(LEDGER_BASE + 0x1000, vec![]));
        assert_eq!(called(&a).0, h.addr("heap_caps_malloc"));
        a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        let mut freed = Vec::new();
        while let HleAction::Call { func, args, .. } = &a {
            assert_eq!(*func, h.addr("heap_caps_free"));
            freed.push(args[0]);
            a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        }
        assert_eq!(
            freed,
            [Arg::Val(LEDGER_BASE + 0x1000), Arg::Val(LEDGER_BASE)]
        );
        assert_eq!(a, ret(ESP_ERR_NO_MEM));
        let state = BleState::decode(&st).unwrap();
        assert!(state.ledger.blocks.is_empty());
        assert_eq!(state.ledger.refused, 1);
        let mut buf = [0u8; 4];
        g.read(STATUS, &mut buf).unwrap();
        assert_eq!(u32::from_le_bytes(buf), STATUS_IDLE);
    }

    #[test]
    fn boot_blocks_outlive_a_deinit_and_the_enable_block_lives_from_enable_to_disable() {
        let mut h = host();
        h.lines_verified = true;
        h.workers(WakeMode::U5Polling);
        let mut g = Guest::default();
        let mut st = Vec::new();
        init_ok(
            &mut h,
            &mut st,
            &mut g,
            0x3FCB_0000,
            0x3FCC_0000,
            0x3FCD_0000,
        );
        let rows = init_rows(&h);
        let plan = h.heap_plan();
        let enable_row = plan
            .iter()
            .find(|r| r.lifetime == Lifetime::Enable)
            .expect("the pk plan has an enable row");

        g.x[10] = 1;
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::ENABLE), &mut g);
        let HleAction::Call { func, args, .. } = &a else {
            panic!("{a:?}")
        };
        assert_eq!(
            (*func, args[0]),
            (h.addr("heap_caps_malloc"), Arg::Val(enable_row.bytes()))
        );
        let a = h.resume(&mut st, &mut hs, &mut g, back(0x3FCF_0000, vec![]));
        let (a, printed) = answer_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert!(printed[0].starts_with("phy_version"), "{printed:?}");
        assert_eq!(a, ret(ESP_OK));
        assert_eq!(
            BleState::decode(&st).unwrap().ledger.blocks.len(),
            rows.len() + 1
        );

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DISABLE), &mut g);
        let (a, freed) = answer_frees(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(freed, [Arg::Val(0x3FCF_0000)]);
        assert_eq!(a, ret(ESP_OK));

        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        let (mut a, freed) = answer_frees(&mut h, &mut st, &mut hs, &mut g, a);
        let at = |i: usize| LEDGER_BASE + (i as u32) * 0x1000;
        let of = |lifetime: Lifetime| -> Vec<u32> {
            rows.iter()
                .enumerate()
                .filter(|(_, r)| r.lifetime == lifetime)
                .map(|(i, _)| at(i))
                .collect()
        };
        let mut init_blocks: Vec<Arg> = of(Lifetime::Init).into_iter().map(Arg::Val).collect();
        init_blocks.reverse();
        assert_eq!(freed, init_blocks);
        while a != ret(ESP_OK) {
            a = h.resume(&mut st, &mut hs, &mut g, back(ESP_OK, vec![]));
        }
        let boot = of(Lifetime::Boot);
        assert!(!boot.is_empty(), "the pk plan has boot rows");
        let held: Vec<u32> = BleState::decode(&st)
            .unwrap()
            .ledger
            .blocks
            .iter()
            .map(|b| b.addr)
            .collect();
        assert_eq!(held, boot);

        let only_init: Vec<&LedgerRow> = rows
            .iter()
            .copied()
            .filter(|r| r.lifetime == Lifetime::Init)
            .collect();
        config(&mut g, 23, 4096, 1, 6);
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        assert_eq!(called(&a).0, h.addr("esp_read_mac"));
        let a = h.resume(
            &mut st,
            &mut hs,
            &mut g,
            back(0, vec![2, 0, 0, 1, 2, 3, 0, 0]),
        );
        let (a, _) = answer_lines(&mut h, &mut st, &mut hs, &mut g, a);
        let a = answer_setup(
            &mut h,
            &mut st,
            &mut hs,
            &mut g,
            a,
            (0x3FCB_1000, 0x3FCC_1000, 0x3FCD_1000),
        );
        let a = answer_ledger(&mut h, &mut st, &mut hs, &mut g, a, &only_init, 0x3FD0_0000);
        assert_eq!(a, ret(ESP_OK));
        assert_eq!(
            BleState::decode(&st).unwrap().ledger.blocks.len(),
            only_init.len() + boot.len()
        );
    }

    #[test]
    fn a_deinit_that_holds_no_interrupt_frees_none() {
        let mut h = host();
        let mut g = Guest::default();
        g.write(STATUS, &STATUS_INITED.to_le_bytes()).unwrap();
        let mut st = BleState {
            worker: 0x3FCC_0000,
            semaphore: 0x3FCB_0000,
            ..BleState::default()
        }
        .encode();
        let (_, a) = h.enter(&mut st, HandlerKind(handler::DEINIT), &mut g);
        assert_eq!(called(&a).0, h.addr("vTaskDelete"));
    }

    #[test]
    fn an_unverified_shape_prints_only_the_lines_no_sdkconfig_decides() {
        // The `Using main XTAL` and `Feature Config` lines are `pk`'s sdkconfig.
        let mut h = host();
        assert!(!h.lines_verified, "no code bytes, no verified shape");
        let mut g = Guest::default();
        config(&mut g, 23, 4096, 1, 6);
        let mut st = Vec::new();
        let (mut hs, _) = h.enter(&mut st, HandlerKind(handler::INIT), &mut g);
        let a = h.resume(
            &mut st,
            &mut hs,
            &mut g,
            back(0, vec![2, 0, 0, 1, 2, 3, 0, 0]),
        );
        let (a, printed) = answer_lines(&mut h, &mut st, &mut hs, &mut g, a);
        assert_eq!(
            printed,
            [
                "BT controller compile version [1bb2f50]",
                "Bluetooth MAC: 02:00:00:01:02:03"
            ]
        );
        let a = answer_setup(
            &mut h,
            &mut st,
            &mut hs,
            &mut g,
            a,
            (0x3FCB_0000, 0x3FCC_0000, 0x3FCD_0000),
        );
        let rows = init_rows(&h);
        assert_eq!(rows.len(), 4);
        let a = answer_ledger(&mut h, &mut st, &mut hs, &mut g, a, &rows, LEDGER_BASE);
        assert_eq!(a, ret(ESP_OK));
        let lines = h.log_lines(&st).expect("the ble module reports its lines");
        assert_eq!((lines.synthesized, lines.verified), (2, false));
    }

    #[test]
    fn init_refuses_what_bt_c_refuses() {
        let mut h = host();
        for (prio, stack, mode, max_act, want) in [
            (22, 4096, 1, 6, ESP_ERR_INVALID_ARG),
            (23, 4095, 1, 6, ESP_ERR_INVALID_ARG),
            (23, 4096, 3, 6, ESP_ERR_NOT_SUPPORTED),
            (23, 4096, 1, 0, ESP_ERR_INVALID_ARG),
            (23, 4096, 1, 11, ESP_ERR_INVALID_ARG),
        ] {
            let mut g = Guest::default();
            config(&mut g, prio, stack, mode, max_act);
            let (_, a) = h.enter(&mut Vec::new(), HandlerKind(handler::INIT), &mut g);
            assert_eq!(a, ret(want), "{prio} {stack} {mode} {max_act}");
        }
        let mut g = Guest::default();
        let (_, a) = h.enter(&mut Vec::new(), HandlerKind(handler::INIT), &mut g);
        assert_eq!(a, ret(ESP_ERR_INVALID_ARG), "a null config");
    }

    #[test]
    fn enable_prints_the_phy_line_once_per_boot_and_the_vhci_follows_the_status() {
        let mut h = host();
        let mut g = Guest::default();
        let mut st = Vec::new();
        g.x[10] = 1;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::ENABLE), &mut g);
        assert_eq!(a, ret(ESP_ERR_INVALID_STATE));
        let (_, a) = h.enter(&mut st, HandlerKind(handler::REGISTER_CB), &mut g);
        assert_eq!(a, ret(ESP_FAIL));
        let (_, a) = h.enter(&mut st, HandlerKind(handler::CHECK_SEND), &mut g);
        assert_eq!(a, ret(0));

        g.write(STATUS, &STATUS_INITED.to_le_bytes()).unwrap();
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::ENABLE), &mut g);
        assert_eq!(called(&a).0, h.addr("esp_log_timestamp"));
        let a = h.resume(&mut st, &mut hs, &mut g, back(410, vec![]));
        assert!(called(&a).1.windows(11).any(|w| w == b"phy_version"));
        let a = h.resume(&mut st, &mut hs, &mut g, back(50, vec![]));
        assert_eq!(a, ret(ESP_OK));
        let (_, a) = h.enter(&mut st, HandlerKind(handler::CHECK_SEND), &mut g);
        assert_eq!(a, ret(1));

        let (_, a) = h.enter(&mut st, HandlerKind(handler::DISABLE), &mut g);
        assert_eq!(a, ret(ESP_OK));
        g.x[10] = 1;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::ENABLE), &mut g);
        assert_eq!(a, ret(ESP_OK));

        g.write(0x3FC9_2000, &[0x11, 0, 0, 0x42, 0x22, 0, 0, 0x42])
            .unwrap();
        g.x[10] = 0x3FC9_2000;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::REGISTER_CB), &mut g);
        assert_eq!(a, ret(ESP_OK));
        g.write(0x3FC9_3000, &[0x01, 0x03, 0x0C, 0x00]).unwrap();
        g.x[10] = 0x3FC9_3000;
        g.x[11] = 4;
        let (mut hs, a) = h.enter(&mut st, HandlerKind(handler::SEND_PACKET), &mut g);
        assert_eq!(called(&a).0, 0x4200_0011);
        assert_eq!(h.resume(&mut st, &mut hs, &mut g, back(0, vec![])), ret(0));
        let state = BleState::decode(&st).unwrap();
        assert_eq!(state.tx_packets, 1);
        assert_eq!(state.recent_tx, [vec![0x01, 0x03, 0x0C, 0x00]]);
        assert!(state.phy_logged);

        let reply = u64::from(BleProfile::load().controller.reset_reply_us);
        assert_ne!(reply, u64::from(BleProfile::load().controller.reply_us));
        assert_eq!(
            state.outbox,
            [Outgoing {
                due_us: reply,
                packet: vec![0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
            }]
        );
        let due = g.sched.pending();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, VTime::from_us(reply));
        assert_eq!(
            due[0].2,
            module_timer(ModuleIndex::FIRST_MODULE, TIMER_CONTROLLER)
        );
        g.now = VTime::from_us(reply - 1);
        assert!(
            h.on_timer(&mut st, TIMER_CONTROLLER, &mut g).is_empty(),
            "not yet due"
        );
        g.now = VTime::from_us(reply);
        assert!(
            h.on_timer(&mut st, TIMER_CONTROLLER + 1, &mut g).is_empty(),
            "another tag"
        );
        let events = h.on_timer(&mut st, TIMER_CONTROLLER, &mut g);
        assert_eq!(
            events,
            [RadioEvent {
                tag: TIMER_CONTROLLER,
                payload: vec![0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
            }]
        );
        assert!(BleState::decode(&st).unwrap().outbox.is_empty());
    }

    #[test]
    fn the_worker_parks_and_delivers_each_event_to_notify_host_recv() {
        let mut h = host();
        let mut g = Guest::default();
        let mut st = BleState {
            cb_recv: 0x4200_0022,
            ..BleState::default()
        }
        .encode();
        let worker = HandlerKind(u16::MAX - MagicKind::BtWorker as u16);
        let (mut hs, a) = h.enter(&mut st, worker, &mut g);
        assert_eq!((hs.handler.as_str(), a), ("ble.worker", HleAction::Park));
        let event = RadioEvent {
            tag: 0,
            payload: vec![0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00],
        };
        h.deliver(&mut st, MagicKind::BtWorker, vec![event.clone(), event]);
        let woken = Resume::Woken {
            reason: WakeReason::Event,
        };
        let a = h.resume(&mut st, &mut hs, &mut g, woken);
        let HleAction::Call {
            func,
            args,
            scratch,
            ..
        } = &a
        else {
            panic!("{a:?}")
        };
        assert_eq!((*func, args[1]), (0x4200_0022, Arg::Val(7)));
        assert_eq!(&scratch[..7], &[0x04, 0x0E, 0x04, 0x05, 0x03, 0x0C, 0x00]);
        let a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        assert!(matches!(a, HleAction::Call { .. }));
        let a = h.resume(&mut st, &mut hs, &mut g, back(0, vec![]));
        assert_eq!(a, HleAction::Park);
        assert_eq!(BleState::decode(&st).unwrap().rx_packets, 2);
    }

    #[test]
    fn a_host_packet_longer_than_an_h4_packet_is_refused_not_truncated() {
        let mut h = host();
        let mut g = Guest::default();
        g.write(STATUS, &STATUS_ENABLED.to_le_bytes()).unwrap();
        let mut st = BleState {
            cb_send_available: 0x4200_0011,
            ..BleState::default()
        }
        .encode();
        g.write(0x3FC9_3000, &[0x02; 300]).unwrap();
        g.x[10] = 0x3FC9_3000;
        g.x[11] = MAX_H4 + 1;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SEND_PACKET), &mut g);
        assert_eq!(a, ret(0), "no nested call: the sender is not re-armed");
        let state = BleState::decode(&st).unwrap();
        assert_eq!((state.tx_packets, state.refused_tx), (0, 1));
        assert!(state.recent_tx.is_empty());

        g.x[11] = MAX_H4;
        let (_, a) = h.enter(&mut st, HandlerKind(handler::SEND_PACKET), &mut g);
        assert_eq!(called(&a).0, 0x4200_0011, "a 259-byte packet is taken");
        let state = BleState::decode(&st).unwrap();
        assert_eq!((state.tx_packets, state.refused_tx), (1, 1));
    }

    #[test]
    fn the_worker_hands_a_whole_259_byte_packet_within_its_scratch_limit() {
        let mut h = host();
        let limit = h
            .workers(WakeMode::U5Polling)
            .first()
            .and_then(|w| w.profile.max_scratch)
            .expect("the BLE worker sets its scratch limit");
        let mut g = Guest::default();
        let mut st = BleState {
            cb_recv: 0x4200_0022,
            ..BleState::default()
        }
        .encode();
        let worker = HandlerKind(u16::MAX - MagicKind::BtWorker as u16);
        let (mut hs, _) = h.enter(&mut st, worker, &mut g);
        let big = RadioEvent {
            tag: 0,
            payload: vec![0x04; MAX_H4 as usize],
        };
        let too_big = RadioEvent {
            tag: 0,
            payload: vec![0x04; MAX_H4 as usize + 1],
        };
        h.deliver(&mut st, MagicKind::BtWorker, vec![too_big, big]);
        assert_eq!(
            BleState::decode(&st).unwrap().dropped_rx,
            1,
            "the oversize event"
        );
        let woken = Resume::Woken {
            reason: WakeReason::Event,
        };
        let a = h.resume(&mut st, &mut hs, &mut g, woken);
        let HleAction::Call { args, scratch, .. } = &a else {
            panic!("{a:?}")
        };
        assert_eq!(args[1], Arg::Val(MAX_H4));
        assert_eq!(scratch.len(), 260);
        assert!(scratch.len() <= usize::from(limit));
        assert!(
            scratch.len() > usize::from(pemu_hle::guest_call::GuardProfile::default().max_scratch)
        );
    }

    #[test]
    fn the_state_round_trips_and_trailing_bytes_are_refused() {
        let st = BleState {
            worker: 1,
            pending_rx: vec![vec![1, 2, 3]],
            recent_tx: vec![vec![9]],
            phy_logged: true,
            ..BleState::default()
        };
        assert_eq!(BleState::decode(&st.encode()).unwrap(), st);
        let st = BleState {
            refused_tx: 3,
            ..st
        };
        assert_eq!(BleState::decode(&st.encode()).unwrap(), st);
        let mut bytes = st.encode();
        bytes.push(0);
        assert!(BleState::decode(&bytes).is_err());
    }
}
