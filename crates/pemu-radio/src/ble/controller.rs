//! The virtual LE controller behind the VHCI handlers: every H4 packet the host sends is answered
//! here, as events delivered in virtual time, never through a synchronous wait. Written from the
//! Bluetooth Core Specification 5.4 Vol 4 Part E.
//!
//! - Every command gets exactly one acknowledgement with the profile's `Num_HCI_Command_Packets`:
//!   Command Complete when synchronous, Command Status when asynchronous (`hci::ASYNC_OPCODES`).
//!   `HCI_Host_Number_Of_Completed_Packets` is never acknowledged when it succeeds (§7.3.40), nor,
//!   as on the device, when it has no parameters.
//! - `ble.toml` `[[command]]` rows are answered by their `answer`. Any other opcode gets Unknown
//!   HCI Command, and a vendor-specific one (OGF 0x3F) success. No opcode goes unanswered.
//! - Legacy and extended advertising commands (§3.1.1 Table 3.2) do not mix: after one family, the
//!   other is Command Disallowed until `HCI_Reset`, checked before the command's own parameters
//!   (the device answered 0x2007 after 0x203A and 0x203B that way).
//! - Completion events other than acknowledgements honor `Event_Mask` and `LE_Event_Mask`.
//! - A host ACL packet waits in [`Connection::tx`] for the next connection event on the air, where
//!   it is transmitted and `HCI_Number_Of_Completed_Packets` reports it.
//!
//! Connections come from the air ([`Controller::accept_connection`]). `HCI_LE_Create_Connection`
//! stays pending until cancelled, because no virtual peripheral advertises on this air.

use pemu_core::snap::snap_struct;

use super::hci::{self, Acl, Command, mask, status, subevent};
use super::profile::{Answering, BleProfile, ControllerRow};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Connection {
    pub handle: u16,
    /// 1.25 ms units.
    pub interval: u16,
    pub latency: u16,
    /// 10 ms units.
    pub timeout: u16,
    /// `Max_TX_Octets` and `Max_TX_Time` in force.
    pub tx_octets: u16,
    pub tx_time: u16,
    /// 1 is LE 1M, 2 is LE 2M.
    pub tx_phy: u8,
    pub rx_phy: u8,
    /// Whole H4 packets waiting for the next connection event, oldest first.
    pub tx: Vec<Vec<u8>>,
    /// The reason of an `HCI_Disconnect` whose `LL_TERMINATE_IND` the air has not completed yet.
    pub terminating: Option<u8>,
    /// An `HCI_LE_Long_Term_Key_Request` awaits a reply (§7.7.65.5, §7.8.25, §7.8.26).
    pub ltk_requested: bool,
    /// 10 ms units (§7.3.94).
    pub auth_payload_timeout: u16,
}

/// 30 s in 10 ms units (Core Vol 6 Part B §5.4).
pub const AUTH_PAYLOAD_TIMEOUT: u16 = 3000;

snap_struct!(Connection {
    handle,
    interval,
    latency,
    timeout,
    tx_octets,
    tx_time,
    auth_payload_timeout,
    tx_phy,
    rx_phy,
    ltk_requested,
    tx,
    terminating,
});

/// The legacy advertising state (Core Vol 4 Part E §7.8.5 to §7.8.9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advertising {
    /// The 15 parameter bytes of the last accepted `HCI_LE_Set_Advertising_Parameters`.
    pub params: [u8; 15],
    pub data: Vec<u8>,
    pub scan_response: Vec<u8>,
    pub enabled: bool,
}

impl Default for Advertising {
    fn default() -> Advertising {
        let mut params = [0u8; 15];
        // §7.8.5 defaults: intervals 0x0800 (1.28 s), ADV_IND, public own address, all channels, no
        // filter.
        params[0..2].copy_from_slice(&0x0800u16.to_le_bytes());
        params[2..4].copy_from_slice(&0x0800u16.to_le_bytes());
        params[13] = 0x07;
        Advertising {
            params,
            data: Vec::new(),
            scan_response: Vec::new(),
            enabled: false,
        }
    }
}

impl Advertising {
    /// 0.625 ms units.
    pub fn interval(&self) -> (u16, u16) {
        (
            u16::from_le_bytes([self.params[0], self.params[1]]),
            u16::from_le_bytes([self.params[2], self.params[3]]),
        )
    }

    pub fn kind(&self) -> u8 {
        self.params[4]
    }

    pub fn own_address_type(&self) -> u8 {
        self.params[5]
    }
}

/// The controller state. It is part of the BLE module state, so a snapshot between a command and
/// its answer restores the answer still queued.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Controller {
    pub event_mask: u64,
    pub event_mask_page2: u64,
    pub le_event_mask: u64,
    /// Least significant octet first.
    pub random_address: [u8; 6],
    pub advertising: Advertising,
    pub scan_params: [u8; 7],
    pub scan_enable: [u8; 2],
    pub initiating: bool,
    pub initiator_filter: u8,
    /// Address type and address.
    pub accept_list: Vec<[u8; 7]>,
    /// Identity address type, address and privacy mode.
    pub resolving_list: Vec<[u8; 8]>,
    pub address_resolution: bool,
    /// Seconds.
    pub rpa_timeout: u16,
    pub suggested_data_length: [u16; 2],
    pub default_phy: [u8; 3],
    pub host_flow_control: u8,
    pub connections: Vec<Connection>,
    /// The advertising family issued since power-on or reset (§3.1.1): 0, [`LEGACY`] or
    /// [`EXTENDED`].
    pub advertising_family: u8,
    /// Direct Test Mode test in progress: 0, [`RECEIVER_TEST`] or [`TRANSMITTER_TEST`].
    pub test: u8,
    /// In 0.1 dB, as sent (§7.8.75, §7.8.76).
    pub rf_path_compensation: [u16; 2],
    pub commands: u64,
    pub unknown_commands: u64,
    pub acl_packets: u64,
    /// Packets the air took and has not reported completed: controller buffers in use.
    pub acl_in_flight: u16,
    /// No such connection, or malformed.
    pub acl_dropped: u64,
    /// Diagnostic: the last [`RECENT_ANSWERS`] acknowledgements as opcode (little-endian) and
    /// status.
    pub recent: Vec<[u8; 3]>,
}

pub const LEGACY: u8 = 1;
pub const EXTENDED: u8 = 2;
pub const RECEIVER_TEST: u8 = 1;
pub const TRANSMITTER_TEST: u8 = 2;

pub const RECENT_ANSWERS: usize = 32;

impl Default for Controller {
    fn default() -> Controller {
        Controller {
            event_mask: mask::DEFAULT,
            event_mask_page2: 0,
            le_event_mask: mask::LE_DEFAULT,
            random_address: [0; 6],
            advertising: Advertising::default(),
            scan_params: [0x00, 0x10, 0x00, 0x10, 0x00, 0x00, 0x00],
            scan_enable: [0; 2],
            initiating: false,
            initiator_filter: 0,
            accept_list: Vec::new(),
            resolving_list: Vec::new(),
            address_resolution: false,
            // §7.8.45: the default RPA timeout is 900 s.
            rpa_timeout: 900,
            // §7.8.34: 27 octets and 328 us until the host writes others.
            suggested_data_length: [27, 328],
            default_phy: [0; 3],
            host_flow_control: 0,
            connections: Vec::new(),
            advertising_family: 0,
            test: 0,
            // §7.8.76: vendor-specific defaults; 0 dB, class C.
            rf_path_compensation: [0; 2],
            commands: 0,
            unknown_commands: 0,
            acl_packets: 0,
            acl_in_flight: 0,
            acl_dropped: 0,
            recent: Vec::new(),
        }
    }
}

pub type Events = Vec<Vec<u8>>;

fn u16_at(p: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([p[at], p[at + 1]])
}

impl Controller {
    /// Handles one H4 packet from the host and returns its events. `public_address` is least
    /// significant octet first. `entropy` draws from the machine seed's `RngStream::RADIO_BLE`, so
    /// `HCI_LE_Rand` follows the seed and its position is in the `rng` snapshot section.
    pub fn on_packet(
        &mut self,
        public_address: [u8; 6],
        packet: &[u8],
        entropy: &mut dyn FnMut(&mut [u8]),
    ) -> Events {
        match packet.first() {
            Some(&hci::H4_COMMAND) => match Command::parse(packet) {
                Some(cmd) => self.command(public_address, &cmd, entropy),
                // A truncated command header cannot be acknowledged by opcode; VHCI never splits
                // one, so it is a host bug and dropped.
                None => Vec::new(),
            },
            Some(&hci::H4_ACL) => self.acl(packet),
            _ => Vec::new(),
        }
    }

    /// A connection with the defaults of a new LE connection: 27 octets at 328 us, LE 1M both ways
    /// (Core Vol 6 Part B §4.5.10).
    pub fn open_connection(&mut self, handle: u16, interval: u16, latency: u16, timeout: u16) {
        self.connections.retain(|c| c.handle != handle);
        self.connections.push(Connection {
            handle,
            interval,
            latency,
            timeout,
            tx_octets: 27,
            tx_time: 328,
            tx_phy: 1,
            rx_phy: 1,
            tx: Vec::new(),
            terminating: None,
            ltk_requested: false,
            auth_payload_timeout: AUTH_PAYLOAD_TIMEOUT,
        });
    }

    fn connection(&mut self, handle: u16) -> Option<&mut Connection> {
        self.connections.iter_mut().find(|c| c.handle == handle)
    }

    fn le_event_enabled(&self, sub: u8) -> bool {
        hci::mask_has(self.event_mask, mask::LE_META)
            && hci::mask_has(self.le_event_mask, u32::from(sub) - 1)
    }

    /// The `HCI_LE_Long_Term_Key_Request` event (§7.7.65.5) the air sends when a central starts
    /// encryption; marks the request pending. `None` for an unknown handle or a masked event.
    pub fn request_ltk(&mut self, handle: u16, random: [u8; 8], ediv: u16) -> Option<Vec<u8>> {
        let enabled = self.le_event_enabled(subevent::LONG_TERM_KEY_REQUEST);
        let conn = self.connection(handle)?;
        conn.ltk_requested = true;
        let [lo, hi] = handle.to_le_bytes();
        let mut params = vec![lo, hi];
        params.extend_from_slice(&random);
        params.extend_from_slice(&ediv.to_le_bytes());
        enabled.then(|| hci::le_meta(subevent::LONG_TERM_KEY_REQUEST, &params))
    }

    /// Queues a host ACL packet for the next connection event; nothing is answered now. A packet
    /// for no connection, or beyond the announced buffer size or count, is discarded and counted.
    fn acl(&mut self, packet: &[u8]) -> Events {
        let id = &BleProfile::load().controller;
        let (max_len, max_count) = (usize::from(id.acl_len), usize::from(id.acl_count));
        let Some(header) = Acl::parse(packet) else {
            self.acl_dropped += 1;
            return Vec::new();
        };
        if !self.has_connection(header.handle) {
            self.acl_dropped += 1;
            return Vec::new();
        }
        // §4.1.1: a host that overruns the buffers gets its packet discarded, not a buffer the
        // device does not have. `Total_Num_LE_ACL_Data_Packets` counts buffers of the whole
        // controller (§7.8.2), so queued and in-flight packets on any connection both hold one.
        let handle = header.handle;
        if usize::from(header.len) > max_len || self.acl_buffers_in_use() >= max_count {
            self.acl_dropped += 1;
            return Vec::new();
        }
        let packet = packet[..5 + usize::from(header.len)].to_vec();
        if let Some(conn) = self.connection(handle) {
            conn.tx.push(packet);
        }
        self.acl_packets += 1;
        Vec::new()
    }

    pub fn acl_buffers_in_use(&self) -> usize {
        self.connections.iter().map(|c| c.tx.len()).sum::<usize>() + usize::from(self.acl_in_flight)
    }

    /// The lowest free handle from 0x0001 (Core Vol 4 Part E §5.3). Which one the device hands out
    /// first is UNVERIFIED, class C.
    pub fn free_handle(&self) -> u16 {
        (1..0x0EFF)
            .find(|h| self.connections.iter().all(|c| c.handle != *h))
            .unwrap_or(0x0EFF)
    }

    /// A central's `CONNECT_IND`: advertising ends and the connection opens. The event is the
    /// newest form the masks enable: Enhanced Connection Complete v2 with `LE_Event_Mask` bit 40,
    /// else v1 with bit 9, else `HCI_LE_Connection_Complete` with bit 0 (Vol 4 Part E §7.7.65.1,
    /// .10, .29).
    #[allow(clippy::too_many_arguments)]
    pub fn accept_connection(
        &mut self,
        handle: u16,
        peer_random: bool,
        peer: [u8; 6],
        interval: u16,
        latency: u16,
        timeout: u16,
        sca: u8,
    ) -> Option<Vec<u8>> {
        self.advertising.enabled = false;
        self.open_connection(handle, interval, latency, timeout);
        let [lo, hi] = handle.to_le_bytes();
        // Status, handle, role (0x01 peripheral), peer address type and address.
        let mut head = vec![status::SUCCESS, lo, hi, 0x01, u8::from(peer_random)];
        head.extend_from_slice(&peer);
        let mut tail = Vec::new();
        for v in [interval, latency, timeout] {
            tail.extend_from_slice(&v.to_le_bytes());
        }
        tail.push(sca);
        if self.le_event_enabled(subevent::ENHANCED_CONNECTION_COMPLETE_V2)
            || self.le_event_enabled(subevent::ENHANCED_CONNECTION_COMPLETE)
        {
            let v2 = self.le_event_enabled(subevent::ENHANCED_CONNECTION_COMPLETE_V2);
            let mut params = head;
            // Local and peer resolvable private addresses: none, this controller resolves none.
            params.extend_from_slice(&[0; 12]);
            params.extend_from_slice(&tail);
            if v2 {
                // No advertising set (0xFF, UNVERIFIED which value the device reports) and no
                // periodic sync.
                params.extend_from_slice(&[0xFF, 0xFF, 0xFF]);
                return Some(hci::le_meta(
                    subevent::ENHANCED_CONNECTION_COMPLETE_V2,
                    &params,
                ));
            }
            return Some(hci::le_meta(
                subevent::ENHANCED_CONNECTION_COMPLETE,
                &params,
            ));
        }
        let mut params = head;
        params.extend_from_slice(&tail);
        self.le_event_enabled(subevent::CONNECTION_COMPLETE)
            .then(|| hci::le_meta(subevent::CONNECTION_COMPLETE, &params))
    }

    /// The central applied a connection update (Core Vol 6 Part B §5.1.1); returns
    /// `HCI_LE_Connection_Update_Complete` when the masks enable it.
    pub fn peer_update(
        &mut self,
        handle: u16,
        interval: u16,
        latency: u16,
        timeout: u16,
    ) -> Option<Vec<u8>> {
        let conn = self.connection(handle)?;
        conn.interval = interval;
        conn.latency = latency;
        conn.timeout = timeout;
        let conn = conn.clone();
        self.update_complete(&conn)
    }

    /// High duty cycle directed advertising ended unanswered after 1.28 s (§7.8.9): advertising
    /// stops and the connection complete event carries Advertising Timeout.
    pub fn advertising_timeout(&mut self) -> Option<Vec<u8>> {
        self.advertising.enabled = false;
        self.connection_failed(status::ADVERTISING_TIMEOUT)
    }

    /// The buffers stay in use until [`Controller::completed`] reports them.
    pub fn take_transmit(&mut self, handle: u16, max: usize) -> Vec<Vec<u8>> {
        let taken = match self.connection(handle) {
            Some(conn) => {
                let n = conn.tx.len().min(max);
                conn.tx.drain(..n).collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        self.acl_in_flight = self.acl_in_flight.saturating_add(taken.len() as u16);
        taken
    }

    /// Packets the air took are gone with their connection: buffers free, and no
    /// `HCI_Number_Of_Completed_Packets`, which reports live connections only (§7.7.19).
    pub fn drop_in_flight(&mut self, count: u16) {
        self.acl_in_flight = self.acl_in_flight.saturating_sub(count);
    }

    pub fn completed(&mut self, handle: u16, count: u16) -> Option<Vec<u8>> {
        if count == 0 {
            return None;
        }
        self.acl_in_flight = self.acl_in_flight.saturating_sub(count);
        Some(hci::number_of_completed_packets(&[(handle, count)]))
    }

    pub fn has_connection(&self, handle: u16) -> bool {
        self.connections.iter().any(|c| c.handle == handle)
    }

    /// The peer sent `LL_TERMINATE_IND`: the connection and its untransmitted packets are gone.
    /// `None` for an unknown handle or a masked event.
    pub fn remote_disconnect(&mut self, handle: u16, reason: u8) -> Option<Vec<u8>> {
        if !self.has_connection(handle) {
            return None;
        }
        self.connections.retain(|c| c.handle != handle);
        let [lo, hi] = handle.to_le_bytes();
        hci::mask_has(self.event_mask, mask::DISCONNECTION_COMPLETE).then(|| {
            hci::event(
                hci::event::DISCONNECTION_COMPLETE,
                &[status::SUCCESS, lo, hi, reason],
            )
        })
    }
}

enum Answer {
    Complete(Vec<u8>),
    CompleteThen(Vec<u8>, Option<Vec<u8>>),
    Status(u8, Option<Vec<u8>>),
    Silent,
}

fn done(code: u8) -> Answer {
    Answer::Complete(vec![code])
}

fn done_handle(code: u8, handle: u16) -> Answer {
    let [lo, hi] = handle.to_le_bytes();
    Answer::Complete(vec![code, lo, hi])
}

/// The `Supported_Commands` bitmap of the rows the controller answers. `ble.toml` also states the
/// device's bitmap; the profile test checks the two agree.
pub fn row_bitmap(profile: &BleProfile) -> [u8; 64] {
    let mut map = [0u8; 64];
    for row in profile
        .commands
        .iter()
        .filter(|c| c.answer != Answering::Unknown)
    {
        if let Some((octet, bit)) = row.supported {
            hci::set_bit(&mut map, usize::from(octet) * 8 + usize::from(bit));
        }
    }
    map
}

/// The advertising family of `op` (Core Vol 4 Part E §3.1.1 Table 3.2), 0 for neither.
pub fn advertising_family(op: u16) -> u8 {
    match op {
        0x2006..=0x200D => LEGACY,
        0x2036..=0x204A | 0x205C | 0x205D => EXTENDED,
        _ => 0,
    }
}

fn refuse(op: u16, code: u8) -> Answer {
    if hci::is_async(op) {
        Answer::Status(code, None)
    } else {
        done(code)
    }
}

impl Controller {
    fn command(
        &mut self,
        public_address: [u8; 6],
        cmd: &Command<'_>,
        entropy: &mut dyn FnMut(&mut [u8]),
    ) -> Events {
        self.commands += 1;
        let profile = BleProfile::load();
        let id = &profile.controller;
        let op = cmd.opcode;
        let row = profile.commands.iter().find(|c| c.opcode == op);
        let family = advertising_family(op);
        let answer = match row.map(|r| r.answer) {
            Some(Answering::Unknown) => {
                // The device answers these with a Command Complete carrying only the status.
                self.unknown_commands += 1;
                done(status::UNKNOWN_COMMAND)
            }
            Some(_) if family != 0 && self.advertising_family & !family != 0 => {
                if op == 0x2007 {
                    // The device's refusal carries the power byte as well.
                    Answer::Complete(vec![status::COMMAND_DISALLOWED, id.adv_tx_power as u8])
                } else {
                    refuse(op, status::COMMAND_DISALLOWED)
                }
            }
            Some(answer) => {
                if family != 0 {
                    self.advertising_family = family;
                }
                if answer == Answering::Unmodeled {
                    refuse(op, status::UNSUPPORTED_FEATURE)
                } else if op == 0x2018 && cmd.params.is_empty() {
                    // §7.8.23: 8 octets from the seed's RADIO_BLE stream.
                    let mut ret = [0u8; 9];
                    entropy(&mut ret[1..]);
                    Answer::Complete(ret.to_vec())
                } else {
                    self.known(profile, public_address, op, cmd.params)
                }
            }
            None if op >> 10 == hci::OGF_VENDOR => done(status::SUCCESS),
            None => {
                self.unknown_commands += 1;
                refuse(op, status::UNKNOWN_COMMAND)
            }
        };
        let code = match &answer {
            Answer::Complete(ret) | Answer::CompleteThen(ret, _) => ret.first().copied(),
            Answer::Status(code, _) => Some(*code),
            Answer::Silent => None,
        };
        if let Some(code) = code {
            let [lo, hi] = op.to_le_bytes();
            self.recent.push([lo, hi, code]);
            if self.recent.len() > RECENT_ANSWERS {
                self.recent.remove(0);
            }
        }
        let num = id.num_command_packets;
        match answer {
            Answer::Complete(ret) => vec![hci::command_complete(num, op, &ret)],
            Answer::CompleteThen(ret, then) => {
                let mut out = vec![hci::command_complete(num, op, &ret)];
                out.extend(then);
                out
            }
            Answer::Status(code, then) => {
                let mut out = vec![hci::command_status(num, code, op)];
                out.extend(then);
                out
            }
            Answer::Silent => Vec::new(),
        }
    }

    /// A `ble.toml` command; a parameter length other than the specification's is Invalid HCI
    /// Command Parameters.
    fn known(
        &mut self,
        profile: &BleProfile,
        public_address: [u8; 6],
        op: u16,
        p: &[u8],
    ) -> Answer {
        let id = &profile.controller;
        let async_op = hci::is_async(op);
        if op == 0x0C35 && p.is_empty() {
            // No Num_Handles at all: the device sends no event, so neither does this controller.
            return Answer::Silent;
        }
        let Some(want) = param_len(op, p) else {
            return if async_op {
                Answer::Status(status::INVALID_PARAMETERS, None)
            } else {
                done(status::INVALID_PARAMETERS)
            };
        };
        debug_assert_eq!(want, p.len());
        match op {
            0x0406 => self.disconnect(u16_at(p, 0), p[2]),
            0x041D => self.remote_version(id, u16_at(p, 0)),
            0x0C01 => {
                self.event_mask = u64::from_le_bytes(p.try_into().unwrap_or_default());
                done(status::SUCCESS)
            }
            0x0C03 => {
                // §7.3.2: the Link Layer returns to its default state; connections end without
                // events. The generator and the counters are this emulator's, not the LL's.
                let kept = self.clone();
                *self = Controller {
                    commands: kept.commands,
                    unknown_commands: kept.unknown_commands,
                    acl_packets: kept.acl_packets,
                    acl_dropped: kept.acl_dropped,
                    recent: kept.recent,
                    ..Controller::default()
                };
                done(status::SUCCESS)
            }
            0x0C2D => {
                let handle = u16_at(p, 0);
                let [lo, hi] = handle.to_le_bytes();
                if p[2] > 1 {
                    return Answer::Complete(vec![status::INVALID_PARAMETERS, lo, hi, 0]);
                }
                if self.connection(handle).is_none() {
                    return Answer::Complete(vec![status::UNKNOWN_CONNECTION, lo, hi, 0]);
                }
                // §7.3.35: range -30 to +20 dBm. Current is the profile's advertising power (class
                // B); maximum is the device's `Max_TX_Power` clamped to that range.
                let level = if p[2] == 0 {
                    id.adv_tx_power
                } else {
                    id.tx_power_max.min(20)
                };
                Answer::Complete(vec![status::SUCCESS, lo, hi, level as u8])
            }
            0x0C7B => {
                let handle = u16_at(p, 0);
                let [lo, hi] = handle.to_le_bytes();
                match self.connection(handle) {
                    Some(c) => {
                        let [t0, t1] = c.auth_payload_timeout.to_le_bytes();
                        Answer::Complete(vec![status::SUCCESS, lo, hi, t0, t1])
                    }
                    None => Answer::Complete(vec![status::UNKNOWN_CONNECTION, lo, hi, 0, 0]),
                }
            }
            0x0C7C => {
                let (handle, timeout) = (u16_at(p, 0), u16_at(p, 2));
                let Some(c) = self.connection(handle) else {
                    return done_handle(status::UNKNOWN_CONNECTION, handle);
                };
                // §7.3.94: at least connInterval x (1 + latency) (subrate factor 1). In 10 ms and
                // 1.25 ms units: timeout x 8 >= interval x (1 + latency).
                let floor = u32::from(c.interval) * (1 + u32::from(c.latency));
                if timeout == 0 || u32::from(timeout) * 8 < floor {
                    return done_handle(status::INVALID_PARAMETERS, handle);
                }
                c.auth_payload_timeout = timeout;
                done_handle(status::SUCCESS, handle)
            }
            0x0C31 => {
                if p[0] > 3 {
                    return done(status::INVALID_PARAMETERS);
                }
                self.host_flow_control = p[0];
                done(status::SUCCESS)
            }
            0x0C33 => done(status::SUCCESS),
            0x0C35 => {
                // §7.3.40: no event unless the parameters are wrong, then Invalid HCI Command
                // Parameters.
                let handles = usize::from(p[0]);
                if p.len() != 1 + 4 * handles {
                    return done(status::INVALID_PARAMETERS);
                }
                Answer::Silent
            }
            0x0C63 => {
                self.event_mask_page2 = u64::from_le_bytes(p.try_into().unwrap_or_default());
                done(status::SUCCESS)
            }
            0x1001 => {
                let mut ret = vec![status::SUCCESS, id.hci_version];
                ret.extend_from_slice(&id.hci_subversion.to_le_bytes());
                ret.push(id.lmp_version);
                ret.extend_from_slice(&id.company.to_le_bytes());
                ret.extend_from_slice(&id.lmp_subversion.to_le_bytes());
                Answer::Complete(ret)
            }
            0x1002 => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.supported_commands);
                Answer::Complete(ret)
            }
            0x1003 => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.lmp_features);
                Answer::Complete(ret)
            }
            0x1009 => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&public_address);
                Answer::Complete(ret)
            }
            0x1405 => {
                let handle = u16_at(p, 0);
                let [lo, hi] = handle.to_le_bytes();
                match self.connection(handle) {
                    // The peer's RSSI belongs to the air: -40 dBm is a class C placeholder.
                    Some(_) => Answer::Complete(vec![status::SUCCESS, lo, hi, (-40i8) as u8]),
                    None => Answer::Complete(vec![status::UNKNOWN_CONNECTION, lo, hi, 127]),
                }
            }
            _ => self.le(id, op, p),
        }
    }

    /// `HCI_Disconnect` (§7.1.6): the host hears `HCI_Disconnection_Complete` only once the peer
    /// acknowledges the `LL_TERMINATE_IND` ([`Controller::complete_disconnect`]).
    fn disconnect(&mut self, handle: u16, reason: u8) -> Answer {
        // §7.1.6: the reasons a host may give.
        if ![0x05, 0x13, 0x14, 0x15, 0x1A, 0x29, 0x3B].contains(&reason) {
            return Answer::Status(status::INVALID_PARAMETERS, None);
        }
        let Some(conn) = self.connection(handle) else {
            return Answer::Status(status::UNKNOWN_CONNECTION, None);
        };
        if conn.terminating.is_some() {
            // Already terminating (Vol 1 Part F §2.12).
            return Answer::Status(status::COMMAND_DISALLOWED, None);
        }
        conn.terminating = Some(reason);
        Answer::Status(status::SUCCESS, None)
    }

    pub fn terminating(&self, handle: u16) -> Option<u8> {
        self.connections
            .iter()
            .find(|c| c.handle == handle)
            .and_then(|c| c.terminating)
    }

    /// The peer acknowledged our `LL_TERMINATE_IND`: `HCI_Disconnection_Complete` reports
    /// Connection Terminated By Local Host (§7.7.5) when `Event_Mask` bit 4 enables it.
    pub fn complete_disconnect(&mut self, handle: u16) -> Option<Vec<u8>> {
        if !self.has_connection(handle) {
            return None;
        }
        self.connections.retain(|c| c.handle != handle);
        let [lo, hi] = handle.to_le_bytes();
        hci::mask_has(self.event_mask, mask::DISCONNECTION_COMPLETE).then(|| {
            hci::event(
                hci::event::DISCONNECTION_COMPLETE,
                &[status::SUCCESS, lo, hi, status::LOCAL_HOST_TERMINATED],
            )
        })
    }

    fn remote_version(&mut self, id: &ControllerRow, handle: u16) -> Answer {
        if self.connection(handle).is_none() {
            return Answer::Status(status::UNKNOWN_CONNECTION, None);
        }
        // The peer is a virtual central reporting the same identity as the controller.
        let [lo, hi] = handle.to_le_bytes();
        let mut params = vec![status::SUCCESS, lo, hi, id.lmp_version];
        params.extend_from_slice(&id.company.to_le_bytes());
        params.extend_from_slice(&id.lmp_subversion.to_le_bytes());
        let then = hci::mask_has(self.event_mask, mask::READ_REMOTE_VERSION_COMPLETE)
            .then(|| hci::event(hci::event::READ_REMOTE_VERSION_COMPLETE, &params));
        Answer::Status(status::SUCCESS, then)
    }
}

/// The specification's parameter length for `op`, when `p` has it. Variable-length commands check
/// their own inner lengths.
fn param_len(op: u16, p: &[u8]) -> Option<usize> {
    let want = match op {
        0x0406 => 3,
        0x041D | 0x0C7B | 0x1405 | 0x2015 | 0x2016 | 0x201B | 0x2030 => 2,
        0x0C01 | 0x0C63 | 0x2001 | 0x204E => 8,
        0x0C03 | 0x1001 | 0x1002 | 0x1003 | 0x1009 | 0x2002 | 0x2003 | 0x2007 | 0x200E | 0x200F
        | 0x2010 | 0x2018 | 0x201C | 0x201F | 0x2023 | 0x2029 | 0x202A | 0x202F | 0x203A
        | 0x203B | 0x204B | 0x204C => 0,
        0x0C31 | 0x200A | 0x201D | 0x202D => 1,
        0x0C2D | 0x201E | 0x2033 => 3,
        0x0C7C | 0x2034 | 0x204D => 4,
        0x0C33 => 7,
        0x0C35 => return (!p.is_empty()).then_some(p.len()),
        0x2005 => 6,
        0x2014 => 5,
        0x2006 => 15,
        0x2008 | 0x2009 => 32,
        0x200B | 0x2011 | 0x2012 | 0x2028 | 0x202B | 0x202C => 7,
        0x200C | 0x202E => 2,
        0x200D => 25,
        0x2013 | 0x2020 => 14,
        0x2017 => 32,
        0x2019 => 28,
        0x201A => 18,
        0x2021 => 3,
        0x2022 => 6,
        0x2024 => 4,
        0x2027 => 39,
        0x2031 => 3,
        0x2032 => 7,
        _ => return None,
    };
    (p.len() == want).then_some(want)
}

/// §7.8.18: intervals 7.5 ms to 4 s, latency to 499, timeout 100 ms to 32 s and longer than
/// `(1 + latency) * interval_max * 2`.
fn connection_params_valid(min: u16, max: u16, latency: u16, timeout: u16) -> bool {
    (0x0006..=0x0C80).contains(&min)
        && (0x0006..=0x0C80).contains(&max)
        && min <= max
        && latency <= 0x01F3
        && (0x000A..=0x0C80).contains(&timeout)
        && u32::from(timeout) * 4 > (1 + u32::from(latency)) * u32::from(max)
}

impl Controller {
    fn busy(&self) -> bool {
        self.advertising.enabled || self.scan_enable[0] != 0 || self.initiating
    }

    fn update_complete(&self, conn: &Connection) -> Option<Vec<u8>> {
        let [lo, hi] = conn.handle.to_le_bytes();
        let mut params = vec![status::SUCCESS, lo, hi];
        for v in [conn.interval, conn.latency, conn.timeout] {
            params.extend_from_slice(&v.to_le_bytes());
        }
        self.le_event_enabled(subevent::CONNECTION_UPDATE_COMPLETE)
            .then(|| hci::le_meta(subevent::CONNECTION_UPDATE_COMPLETE, &params))
    }

    /// Encryption Change v2 when `Event_Mask_Page_2` bit 25 is set, else v1 when `Event_Mask` bit 7
    /// is (§7.3.69, §7.7.8). An LE link uses AES-CCM, `Encryption_Enabled` 0x01, a 16-octet key.
    fn encryption_change(&self, handle: u16) -> Option<Vec<u8>> {
        let [lo, hi] = handle.to_le_bytes();
        if hci::mask_has(self.event_mask_page2, mask::PAGE2_ENCRYPTION_CHANGE_V2) {
            return Some(hci::event(
                hci::event::ENCRYPTION_CHANGE_V2,
                &[status::SUCCESS, lo, hi, 0x01, 16],
            ));
        }
        hci::mask_has(self.event_mask, mask::ENCRYPTION_CHANGE).then(|| {
            hci::event(
                hci::event::ENCRYPTION_CHANGE,
                &[status::SUCCESS, lo, hi, 0x01],
            )
        })
    }

    /// The Filter Accept List is in use, so it may not change (§7.8.15 to §7.8.17).
    fn accept_list_in_use(&self) -> bool {
        (self.advertising.enabled && self.advertising.params[14] != 0)
            || (self.scan_enable[0] != 0 && self.scan_params[6] != 0)
            || (self.initiating && self.initiator_filter != 0)
    }

    /// The connection-failed event after a cancelled `HCI_LE_Create_Connection` (§7.8.13) or an
    /// advertising timeout, in the newest form the masks enable, as for
    /// [`Controller::accept_connection`].
    fn connection_failed(&self, code: u8) -> Option<Vec<u8>> {
        if self.le_event_enabled(subevent::ENHANCED_CONNECTION_COMPLETE_V2) {
            let mut params = vec![code];
            params.extend_from_slice(&[0; 32]);
            return Some(hci::le_meta(
                subevent::ENHANCED_CONNECTION_COMPLETE_V2,
                &params,
            ));
        }
        if self.le_event_enabled(subevent::ENHANCED_CONNECTION_COMPLETE) {
            let mut params = vec![code];
            params.extend_from_slice(&[0; 29]);
            return Some(hci::le_meta(
                subevent::ENHANCED_CONNECTION_COMPLETE,
                &params,
            ));
        }
        let mut params = vec![code];
        params.extend_from_slice(&[0; 17]);
        self.le_event_enabled(subevent::CONNECTION_COMPLETE)
            .then(|| hci::le_meta(subevent::CONNECTION_COMPLETE, &params))
    }

    fn apply_params(&mut self, handle: u16, p: &[u8]) -> Option<Option<Vec<u8>>> {
        let (min, latency, timeout) = (u16_at(p, 2), u16_at(p, 6), u16_at(p, 8));
        let conn = self.connection(handle)?;
        // Class C: the controller takes the shortest interval the host allows.
        conn.interval = min;
        conn.latency = latency;
        conn.timeout = timeout;
        let conn = conn.clone();
        Some(self.update_complete(&conn))
    }

    fn le(&mut self, id: &ControllerRow, op: u16, p: &[u8]) -> Answer {
        let handle = if p.len() >= 2 { u16_at(p, 0) } else { 0 };
        let [hlo, hhi] = handle.to_le_bytes();
        match op {
            0x2001 => {
                self.le_event_mask = u64::from_le_bytes(p.try_into().unwrap_or_default());
                done(status::SUCCESS)
            }
            0x2002 => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.acl_len.to_le_bytes());
                ret.push(id.acl_count);
                Answer::Complete(ret)
            }
            0x2003 => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.le_features.to_le_bytes());
                Answer::Complete(ret)
            }
            0x2005 => {
                if self.busy() {
                    return done(status::COMMAND_DISALLOWED);
                }
                self.random_address.copy_from_slice(p);
                done(status::SUCCESS)
            }
            0x2006 => {
                if self.advertising.enabled {
                    return done(status::COMMAND_DISALLOWED);
                }
                let (min, max) = (u16_at(p, 0), u16_at(p, 2));
                let kind = p[4];
                // §7.8.5: high duty cycle directed advertising (type 1) ignores the interval range.
                let intervals_ok = kind == 1
                    || ((0x0020..=0x4000).contains(&min)
                        && (0x0020..=0x4000).contains(&max)
                        && min <= max);
                if !intervals_ok
                    || kind > 4
                    || p[5] > 3
                    || p[6] > 1
                    || !(1..=7).contains(&p[13])
                    || p[14] > 3
                {
                    return done(status::INVALID_PARAMETERS);
                }
                self.advertising.params.copy_from_slice(p);
                done(status::SUCCESS)
            }
            0x2007 => Answer::Complete(vec![status::SUCCESS, id.adv_tx_power as u8]),
            0x2008 | 0x2009 => {
                let len = usize::from(p[0]);
                if len > 31 {
                    return done(status::INVALID_PARAMETERS);
                }
                let data = p[1..1 + len].to_vec();
                if op == 0x2008 {
                    self.advertising.data = data;
                } else {
                    self.advertising.scan_response = data;
                }
                done(status::SUCCESS)
            }
            0x200A => {
                if p[0] > 1 {
                    return done(status::INVALID_PARAMETERS);
                }
                if p[0] == 1
                    && self.advertising.own_address_type() == 1
                    && self.random_address == [0; 6]
                {
                    return done(status::INVALID_PARAMETERS);
                }
                self.advertising.enabled = p[0] == 1;
                done(status::SUCCESS)
            }
            0x200B => {
                if self.scan_enable[0] != 0 {
                    return done(status::COMMAND_DISALLOWED);
                }
                let (interval, window) = (u16_at(p, 1), u16_at(p, 3));
                if p[0] > 1
                    || !(0x0004..=0x4000).contains(&interval)
                    || !(0x0004..=0x4000).contains(&window)
                    || window > interval
                    || p[5] > 3
                    || p[6] > 3
                {
                    return done(status::INVALID_PARAMETERS);
                }
                self.scan_params.copy_from_slice(p);
                done(status::SUCCESS)
            }
            0x200C => {
                if p[0] > 1 || p[1] > 1 {
                    return done(status::INVALID_PARAMETERS);
                }
                self.scan_enable.copy_from_slice(p);
                done(status::SUCCESS)
            }
            0x200D => {
                if self.initiating {
                    return Answer::Status(status::COMMAND_DISALLOWED, None);
                }
                if !connection_params_valid(
                    u16_at(p, 13),
                    u16_at(p, 15),
                    u16_at(p, 17),
                    u16_at(p, 19),
                ) {
                    return Answer::Status(status::INVALID_PARAMETERS, None);
                }
                self.initiating = true;
                self.initiator_filter = p[4];
                Answer::Status(status::SUCCESS, None)
            }
            0x200E => {
                if !self.initiating {
                    return done(status::COMMAND_DISALLOWED);
                }
                self.initiating = false;
                // §7.8.13: a connection complete event with Unknown Connection Identifier follows.
                let then = self.connection_failed(status::UNKNOWN_CONNECTION);
                Answer::CompleteThen(vec![status::SUCCESS], then)
            }
            0x200F => Answer::Complete(vec![status::SUCCESS, id.accept_list_size]),
            0x2010 => {
                if self.accept_list_in_use() {
                    return done(status::COMMAND_DISALLOWED);
                }
                self.accept_list.clear();
                done(status::SUCCESS)
            }
            0x2011 | 0x2012 => {
                if self.accept_list_in_use() {
                    return done(status::COMMAND_DISALLOWED);
                }
                if p[0] > 1 && p[0] != 0xFF {
                    return done(status::INVALID_PARAMETERS);
                }
                let mut entry: [u8; 7] = p.try_into().unwrap_or_default();
                if p[0] == 0xFF {
                    // §7.8.16: the address is ignored for anonymous advertisers.
                    entry[1..].fill(0);
                }
                let present = self.accept_list.contains(&entry);
                if op == 0x2011 {
                    if !present {
                        if self.accept_list.len() >= usize::from(id.accept_list_size) {
                            return done(status::MEMORY_CAPACITY_EXCEEDED);
                        }
                        self.accept_list.push(entry);
                    }
                } else {
                    self.accept_list.retain(|e| *e != entry);
                }
                done(status::SUCCESS)
            }
            0x2013 => {
                if !connection_params_valid(u16_at(p, 2), u16_at(p, 4), u16_at(p, 6), u16_at(p, 8))
                {
                    return Answer::Status(status::INVALID_PARAMETERS, None);
                }
                match self.apply_params(handle, p) {
                    Some(then) => Answer::Status(status::SUCCESS, then),
                    None => Answer::Status(status::UNKNOWN_CONNECTION, None),
                }
            }
            0x2014 => done(status::SUCCESS),
            0x2015 => match self.connection(handle) {
                Some(_) => Answer::Complete(vec![
                    status::SUCCESS,
                    hlo,
                    hhi,
                    0xFF,
                    0xFF,
                    0xFF,
                    0xFF,
                    0x1F,
                ]),
                None => Answer::Complete(vec![status::UNKNOWN_CONNECTION, hlo, hhi, 0, 0, 0, 0, 0]),
            },
            0x2016 => {
                if self.connection(handle).is_none() {
                    return Answer::Status(status::UNKNOWN_CONNECTION, None);
                }
                // The virtual central reports the controller's own features.
                let mut params = vec![status::SUCCESS, hlo, hhi];
                params.extend_from_slice(&id.le_features.to_le_bytes());
                let then = self
                    .le_event_enabled(subevent::READ_REMOTE_FEATURES_COMPLETE)
                    .then(|| hci::le_meta(subevent::READ_REMOTE_FEATURES_COMPLETE, &params));
                Answer::Status(status::SUCCESS, then)
            }
            0x2017 => {
                // §7.8.22: parameters are little-endian, so the last octet sent is FIPS-197
                // `key[0]` and `in[0]`; the result goes back the same way.
                let mut key = [0u8; 16];
                let mut block = [0u8; 16];
                for i in 0..16 {
                    key[i] = p[15 - i];
                    block[i] = p[31 - i];
                }
                let out = pemu_core::aes::encrypt_128(&key, &block);
                let mut ret = vec![status::SUCCESS];
                ret.extend(out.iter().rev());
                Answer::Complete(ret)
            }
            0x2019 => {
                if self.connection(handle).is_none() {
                    return Answer::Status(status::UNKNOWN_CONNECTION, None);
                }
                Answer::Status(status::SUCCESS, self.encryption_change(handle))
            }
            // §7.8.25, §7.8.26: without a pending request, Command Disallowed (Vol 1 Part F §2.12).
            0x201A | 0x201B => {
                let then = self.encryption_change(handle);
                match self.connection(handle) {
                    None => done_handle(status::UNKNOWN_CONNECTION, handle),
                    Some(c) if !c.ltk_requested => done_handle(status::COMMAND_DISALLOWED, handle),
                    Some(c) => {
                        c.ltk_requested = false;
                        if op == 0x201A {
                            Answer::CompleteThen(vec![status::SUCCESS, hlo, hhi], then)
                        } else {
                            done_handle(status::SUCCESS, handle)
                        }
                    }
                }
            }
            0x2021 => match self.connection(handle) {
                Some(_) => done_handle(status::SUCCESS, handle),
                None => done_handle(status::UNKNOWN_CONNECTION, handle),
            },
            0x2020 => {
                if !connection_params_valid(u16_at(p, 2), u16_at(p, 4), u16_at(p, 6), u16_at(p, 8))
                {
                    return done_handle(status::INVALID_PARAMETERS, handle);
                }
                match self.apply_params(handle, p) {
                    Some(then) => Answer::CompleteThen(vec![status::SUCCESS, hlo, hhi], then),
                    None => done_handle(status::UNKNOWN_CONNECTION, handle),
                }
            }
            0x2022 => {
                let (octets, time) = (u16_at(p, 2), u16_at(p, 4));
                if !(0x001B..=0x00FB).contains(&octets) || !(0x0148..=0x4290).contains(&time) {
                    return done_handle(status::INVALID_PARAMETERS, handle);
                }
                let (max_octets, max_time) = (id.max_tx_octets, id.max_tx_time);
                let enabled = self.le_event_enabled(subevent::DATA_LENGTH_CHANGE);
                let Some(conn) = self.connection(handle) else {
                    return done_handle(status::UNKNOWN_CONNECTION, handle);
                };
                let (new_octets, new_time) = (octets.min(max_octets), time.min(max_time));
                let changed = (conn.tx_octets, conn.tx_time) != (new_octets, new_time);
                conn.tx_octets = new_octets;
                conn.tx_time = new_time;
                let mut params = vec![hlo, hhi];
                for v in [new_octets, new_time, max_octets, max_time] {
                    params.extend_from_slice(&v.to_le_bytes());
                }
                let then = (changed && enabled)
                    .then(|| hci::le_meta(subevent::DATA_LENGTH_CHANGE, &params));
                Answer::CompleteThen(vec![status::SUCCESS, hlo, hhi], then)
            }
            0x2023 => {
                let mut ret = vec![status::SUCCESS];
                for v in self.suggested_data_length {
                    ret.extend_from_slice(&v.to_le_bytes());
                }
                Answer::Complete(ret)
            }
            0x2024 => {
                let (octets, time) = (u16_at(p, 0), u16_at(p, 2));
                if !(0x001B..=0x00FB).contains(&octets) || !(0x0148..=0x4290).contains(&time) {
                    return done(status::INVALID_PARAMETERS);
                }
                self.suggested_data_length = [octets, time];
                done(status::SUCCESS)
            }
            _ => self.privacy(id, op, p),
        }
    }

    /// A `TX_PHYs` bit set: LE 1M, LE 2M with feature bit 8, LE Coded with bit 11.
    fn phys(id: &ControllerRow) -> u8 {
        let mut phys = 0x01;
        if id.le_features & (1 << 8) != 0 {
            phys |= 0x02;
        }
        if id.le_features & (1 << 11) != 0 {
            phys |= 0x04;
        }
        phys
    }

    /// Direct Test Mode, advertising-set and transmit-power commands.
    fn test_and_power(&mut self, id: &ControllerRow, op: u16, p: &[u8]) -> Answer {
        let phys = Controller::phys(id);
        match op {
            0x201C => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.supported_states);
                Answer::Complete(ret)
            }
            0x201D | 0x201E | 0x2033 | 0x2034 => {
                // Class C, no specification text: a test starts only from an idle Link Layer.
                if self.test != 0 || self.busy() || !self.connections.is_empty() {
                    return done(status::COMMAND_DISALLOWED);
                }
                let transmit = matches!(op, 0x201E | 0x2034);
                if p[0] > 0x27 {
                    return done(status::INVALID_PARAMETERS);
                }
                // §7.8.28, §7.8.29: PHY 1 LE 1M, 2 LE 2M, 3 LE Coded, 4 LE Coded S=2. A missing or
                // reserved PHY is 0x11.
                let phy = match op {
                    0x2033 => p[1],
                    0x2034 => p[3],
                    _ => 1,
                };
                let phy_bit = match phy {
                    1 => 0x01,
                    2 => 0x02,
                    3 => 0x04,
                    4 if transmit => 0x04,
                    _ => 0,
                };
                if phy_bit & phys == 0 {
                    return done(status::UNSUPPORTED_FEATURE);
                }
                if (op == 0x2033 && p[2] > 1) || (transmit && p[2] > 0x07) {
                    return done(status::INVALID_PARAMETERS);
                }
                self.test = if transmit {
                    TRANSMITTER_TEST
                } else {
                    RECEIVER_TEST
                };
                done(status::SUCCESS)
            }
            0x201F => {
                // Class C: no test to end is Command Disallowed. No reference packets reach the
                // virtual receiver, and a transmitter test reports 0 (§7.8.30).
                if self.test == 0 {
                    return Answer::Complete(vec![status::COMMAND_DISALLOWED, 0, 0]);
                }
                self.test = 0;
                Answer::Complete(vec![status::SUCCESS, 0, 0])
            }
            0x203A => {
                let mut ret = vec![status::SUCCESS];
                ret.extend_from_slice(&id.max_adv_data_len.to_le_bytes());
                Answer::Complete(ret)
            }
            0x203B => Answer::Complete(vec![status::SUCCESS, id.adv_sets]),
            0x204B => Answer::Complete(vec![
                status::SUCCESS,
                id.tx_power_min as u8,
                id.tx_power_max as u8,
            ]),
            0x204C => {
                let mut ret = vec![status::SUCCESS];
                for v in self.rf_path_compensation {
                    ret.extend_from_slice(&v.to_le_bytes());
                }
                Answer::Complete(ret)
            }
            0x204D => {
                // §7.8.76: -128.0 dB (0xFB00) to 128.0 dB (0x0500) in 0.1 dB.
                let (tx, rx) = (u16_at(p, 0) as i16, u16_at(p, 2) as i16);
                if !(-1280..=1280).contains(&tx) || !(-1280..=1280).contains(&rx) {
                    return done(status::INVALID_PARAMETERS);
                }
                self.rf_path_compensation = [u16_at(p, 0), u16_at(p, 2)];
                done(status::SUCCESS)
            }
            // A modeled row without an arm fails the table test rather than being answered wrongly
            // here.
            _ => done(status::UNKNOWN_COMMAND),
        }
    }
}

impl Controller {
    /// Resolving-list, privacy and PHY commands.
    fn privacy(&mut self, id: &ControllerRow, op: u16, p: &[u8]) -> Answer {
        // §7.8.38 to §7.8.40, §7.8.77: locked while address resolution is on and the LL is busy.
        let locked = self.address_resolution && self.busy();
        let identity = |p: &[u8]| -> [u8; 7] { p[..7].try_into().unwrap_or_default() };
        let position = |list: &[[u8; 8]], who: [u8; 7]| list.iter().position(|e| e[..7] == who);
        match op {
            0x2027 => {
                if locked {
                    return done(status::COMMAND_DISALLOWED);
                }
                let who = identity(p);
                if p[0] > 1 || position(&self.resolving_list, who).is_some() {
                    return done(status::INVALID_PARAMETERS);
                }
                if self.resolving_list.len() >= usize::from(id.resolving_list_size) {
                    return done(status::MEMORY_CAPACITY_EXCEEDED);
                }
                let mut entry = [0u8; 8];
                entry[..7].copy_from_slice(&who);
                self.resolving_list.push(entry);
                done(status::SUCCESS)
            }
            0x2028 => {
                if locked {
                    return done(status::COMMAND_DISALLOWED);
                }
                match position(&self.resolving_list, identity(p)) {
                    Some(i) => {
                        self.resolving_list.remove(i);
                        done(status::SUCCESS)
                    }
                    None => done(status::UNKNOWN_CONNECTION),
                }
            }
            0x2029 => {
                if locked {
                    return done(status::COMMAND_DISALLOWED);
                }
                self.resolving_list.clear();
                done(status::SUCCESS)
            }
            0x202A => Answer::Complete(vec![status::SUCCESS, id.resolving_list_size]),
            // §7.8.42, §7.8.43: this controller generates no resolvable private address.
            0x202B | 0x202C => Answer::Complete(vec![status::UNKNOWN_CONNECTION, 0, 0, 0, 0, 0, 0]),
            0x202D => {
                if p[0] > 1 {
                    return done(status::INVALID_PARAMETERS);
                }
                if self.busy() {
                    return done(status::COMMAND_DISALLOWED);
                }
                self.address_resolution = p[0] == 1;
                done(status::SUCCESS)
            }
            0x202E => {
                let timeout = u16_at(p, 0);
                if !(0x0001..=0x0E10).contains(&timeout) {
                    return done(status::INVALID_PARAMETERS);
                }
                self.rpa_timeout = timeout;
                done(status::SUCCESS)
            }
            0x202F => {
                let mut ret = vec![status::SUCCESS];
                for v in [
                    id.max_tx_octets,
                    id.max_tx_time,
                    id.max_tx_octets,
                    id.max_tx_time,
                ] {
                    ret.extend_from_slice(&v.to_le_bytes());
                }
                Answer::Complete(ret)
            }
            0x2030 => {
                let handle = u16_at(p, 0);
                let [lo, hi] = handle.to_le_bytes();
                match self.connection(handle) {
                    Some(c) => Answer::Complete(vec![status::SUCCESS, lo, hi, c.tx_phy, c.rx_phy]),
                    None => Answer::Complete(vec![status::UNKNOWN_CONNECTION, lo, hi, 0, 0]),
                }
            }
            0x2031 => {
                // §7.8.48: reserved All_PHYs bits and an empty preference are invalid parameters; a
                // PHY this controller lacks is 0x11. Asymmetric PHYs are not supported (class C),
                // so different TX and RX preferences are 0x11 too.
                let supported = Controller::phys(id);
                let (all, tx, rx) = (p[0], p[1], p[2]);
                if all & !0x03 != 0 || (all & 0x01 == 0 && tx == 0) || (all & 0x02 == 0 && rx == 0)
                {
                    return done(status::INVALID_PARAMETERS);
                }
                if (all & 0x01 == 0 && tx & !supported != 0)
                    || (all & 0x02 == 0 && rx & !supported != 0)
                    || (all == 0 && tx != rx)
                {
                    return done(status::UNSUPPORTED_FEATURE);
                }
                self.default_phy.copy_from_slice(p);
                done(status::SUCCESS)
            }
            0x2032 => {
                let handle = u16_at(p, 0);
                let phys = Controller::phys(id);
                let enabled = self.le_event_enabled(subevent::PHY_UPDATE_COMPLETE);
                let Some(conn) = self.connection(handle) else {
                    return Answer::Status(status::UNKNOWN_CONNECTION, None);
                };
                // §7.8.49: "no preference" keeps the PHY; otherwise LE 2M, then LE 1M, then LE
                // Coded (class C order).
                let pick = |no_pref: bool, allowed: u8, current: u8| {
                    let both = allowed & phys;
                    if no_pref {
                        current
                    } else if both & 0x02 != 0 {
                        2
                    } else if both & 0x01 != 0 || both == 0 {
                        1
                    } else {
                        3
                    }
                };
                conn.tx_phy = pick(p[2] & 0x01 != 0, p[3], conn.tx_phy);
                conn.rx_phy = pick(p[2] & 0x02 != 0, p[4], conn.rx_phy);
                let [lo, hi] = handle.to_le_bytes();
                let params = [status::SUCCESS, lo, hi, conn.tx_phy, conn.rx_phy];
                let then = enabled.then(|| hci::le_meta(subevent::PHY_UPDATE_COMPLETE, &params));
                Answer::Status(status::SUCCESS, then)
            }
            0x204E => {
                if locked {
                    return done(status::COMMAND_DISALLOWED);
                }
                if p[7] > 1 {
                    return done(status::INVALID_PARAMETERS);
                }
                match position(&self.resolving_list, identity(p)) {
                    Some(i) => {
                        self.resolving_list[i][7] = p[7];
                        done(status::SUCCESS)
                    }
                    None => done(status::UNKNOWN_CONNECTION),
                }
            }
            _ => self.test_and_power(id, op, p),
        }
    }
}

snap_struct!(Advertising {
    params,
    data,
    scan_response,
    enabled
});

snap_struct!(Controller {
    event_mask,
    event_mask_page2,
    le_event_mask,
    random_address,
    advertising,
    scan_params,
    scan_enable,
    initiating,
    initiator_filter,
    accept_list,
    resolving_list,
    address_resolution,
    rpa_timeout,
    suggested_data_length,
    default_phy,
    host_flow_control,
    connections,
    advertising_family,
    test,
    rf_path_compensation,
    commands,
    unknown_commands,
    acl_packets,
    acl_in_flight,
    acl_dropped,
    recent,
});

#[cfg(test)]
mod tests {
    use super::*;
    use pemu_core::rng::{DetRng, RngStream};
    use pemu_core::snap::{SnapReader, SnapValue};

    /// A placeholder public address (`docs/secrets.md`).
    const ADDR: [u8; 6] = [0x3D, 0xDD, 0x67, 0x00, 0x00, 0x02];

    fn cmd(op: u16, params: &[u8]) -> Vec<u8> {
        let mut out = vec![hci::H4_COMMAND];
        out.extend_from_slice(&op.to_le_bytes());
        out.push(params.len() as u8);
        out.extend_from_slice(params);
        out
    }

    /// `(event code, status, opcode, return parameters after the status)`.
    fn ack(events: &[Vec<u8>]) -> (u8, u8, u16, Vec<u8>) {
        let e = &events[0];
        assert_eq!(e[0], hci::H4_EVENT);
        assert_eq!(usize::from(e[2]), e.len() - 3, "parameter length");
        match e[1] {
            hci::event::COMMAND_COMPLETE => {
                assert_eq!(e[3], 5, "Num_HCI_Command_Packets");
                (
                    e[1],
                    e[6],
                    u16::from_le_bytes([e[4], e[5]]),
                    e[7..].to_vec(),
                )
            }
            hci::event::COMMAND_STATUS => {
                assert_eq!(e[4], 5, "Num_HCI_Command_Packets");
                (e[1], e[3], u16::from_le_bytes([e[5], e[6]]), Vec::new())
            }
            other => panic!("first event {other:#04x} is no acknowledgement"),
        }
    }

    fn send(c: &mut Controller, op: u16, params: &[u8]) -> Vec<Vec<u8>> {
        c.on_packet(ADDR, &cmd(op, params), &mut |out| out.fill(0xA5))
    }

    fn acl(c: &mut Controller, packet: &[u8]) -> Events {
        c.on_packet(ADDR, packet, &mut |_| unreachable!("ACL draws no entropy"))
    }

    fn status_of(c: &mut Controller, op: u16, params: &[u8]) -> u8 {
        ack(&send(c, op, params)).1
    }

    /// 15 to 30 ms, latency 4, timeout 4 s.
    fn update_params(handle: u16) -> Vec<u8> {
        let mut p = handle.to_le_bytes().to_vec();
        for v in [12u16, 24, 4, 400, 0, 0] {
            p.extend_from_slice(&v.to_le_bytes());
        }
        p
    }

    fn with_le_meta() -> Controller {
        let mut c = Controller::default();
        send(&mut c, 0x0C01, &u64::MAX.to_le_bytes());
        send(&mut c, 0x2001, &u64::MAX.to_le_bytes());
        c.open_connection(1, 24, 0, 72);
        c
    }

    /// The opcodes of a recorded GATT session trace and the startup and advertising commands of
    /// `pk` and the official BLE demo each get exactly one acknowledgement naming the opcode.
    #[test]
    fn the_gt3_and_corpus_opcode_sequences_are_all_acknowledged() {
        let mut adv_params = vec![0xA0, 0, 0xF0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0];
        let mut adv_data = vec![17];
        adv_data.extend_from_slice(&[0x02, 0x01, 0x06, 0x0D, 0x09]);
        adv_data.extend_from_slice(b"FoloPassport");
        adv_data.resize(32, 0);
        let mut resolving = vec![0u8; 39];
        resolving[1..7].copy_from_slice(&ADDR);
        let mut privacy = vec![0u8; 8];
        privacy[1..7].copy_from_slice(&ADDR);
        privacy[7] = 1;
        // Each succeeds but 0x2060, which the device answers with Unknown HCI Command.
        let sequence: Vec<(u16, Vec<u8>, u8)> = vec![
            (0x0C03, vec![], hci::event::COMMAND_COMPLETE),
            (0x1001, vec![], hci::event::COMMAND_COMPLETE),
            (0x1002, vec![], hci::event::COMMAND_COMPLETE),
            (0x1003, vec![], hci::event::COMMAND_COMPLETE),
            (0x0C01, vec![0xFF; 8], hci::event::COMMAND_COMPLETE),
            (0x0C63, vec![0; 8], hci::event::COMMAND_COMPLETE),
            (0x2001, vec![0xFF; 8], hci::event::COMMAND_COMPLETE),
            (0x2002, vec![], hci::event::COMMAND_COMPLETE),
            (0x2060, vec![], hci::event::COMMAND_COMPLETE),
            (0x2003, vec![], hci::event::COMMAND_COMPLETE),
            (0x1009, vec![], hci::event::COMMAND_COMPLETE),
            (0x2018, vec![], hci::event::COMMAND_COMPLETE),
            (0x202D, vec![0], hci::event::COMMAND_COMPLETE),
            (0x2029, vec![], hci::event::COMMAND_COMPLETE),
            (0x200A, vec![0], hci::event::COMMAND_COMPLETE),
            (0x2027, resolving, hci::event::COMMAND_COMPLETE),
            (0x204E, privacy, hci::event::COMMAND_COMPLETE),
            (0x2008, adv_data, hci::event::COMMAND_COMPLETE),
            (0x2009, vec![0; 32], hci::event::COMMAND_COMPLETE),
            (0x2006, adv_params.clone(), hci::event::COMMAND_COMPLETE),
            (0x200A, vec![1], hci::event::COMMAND_COMPLETE),
            (0x2016, vec![1, 0], hci::event::COMMAND_STATUS),
            (
                0x2022,
                vec![1, 0, 0xFB, 0, 0x90, 0x42],
                hci::event::COMMAND_COMPLETE,
            ),
        ];
        let mut c = with_le_meta();
        for (op, params, kind) in sequence {
            let events = send(&mut c, op, &params);
            if op == 0x0C03 {
                // Reset ends every connection (§7.3.2); the air would make a new one.
                assert!(c.connections.is_empty());
                c.open_connection(1, 24, 0, 72);
            }
            let (code, status, got, _) = ack(&events);
            assert_eq!((code, got), (kind, op), "{op:#06x}");
            let want = if op == 0x2060 {
                status::UNKNOWN_COMMAND
            } else {
                status::SUCCESS
            };
            assert_eq!(status, want, "{op:#06x}");
        }
        assert_eq!(c.unknown_commands, 1);
        assert!(c.advertising.enabled);
        assert_eq!(&c.advertising.data[5..], b"FoloPassport");
        adv_params[4] = 3;
        let (_, status, _, _) = ack(&send(&mut c, 0x2006, &adv_params));
        assert_eq!(status, status::COMMAND_DISALLOWED);
    }

    /// Every answer of the controller-only probe capture `device-probe_vhci`, replayed in order to
    /// a fresh controller, byte for byte (class A).
    #[test]
    fn the_device_capture_is_answered_byte_for_byte() {
        let capture: [(u16, &str); 19] = [
            (0x0C03, "040e0405030c00"),
            (0x1001, "040e0c0501100009160009e5021600"),
            (
                0x1002,
                "040e44050210002000800000c000000000e40000002822000000000000040000f7ffff7f00000030f0\
                 ffffffffff07000000000000000000000000000000000000000000000000",
            ),
            (0x1003, "040e0c050310000000000060000000"),
            (0x1005, "040e0405051001"),
            (0x2002, "040e0705022000fb000c"),
            (0x2060, "040e0405602001"),
            (0x2003, "040e0c05032000fff9010800000000"),
            (0x201C, "040e0c051c2000ffffffffff030000"),
            (0x200F, "040e05050f20000c"),
            (0x202A, "040e05052a20000a"),
            (0x202F, "040e0c052f2000fb009042fb009042"),
            (0x203A, "040e06053a20007206"),
            (0x203B, "040e05053b200006"),
            (0x204B, "040e06054b2000e815"),
            (0x2007, "040e050507200c09"),
            (0x0C35, ""),
            (0x20FF, "040e0405ff2001"),
            (0x0CFF, "040e0405ff0c01"),
        ];
        let hex = |s: &str| -> Vec<u8> {
            let s: String = s.split_whitespace().collect();
            (0..s.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
                .collect()
        };
        let mut c = Controller::default();
        for (op, want) in capture {
            let events = send(&mut c, op, &[]);
            let want: Vec<Vec<u8>> = if want.is_empty() {
                Vec::new()
            } else {
                vec![hex(want)]
            };
            assert_eq!(events, want, "{op:#06x}");
        }
        assert_eq!(c.unknown_commands, 4);
    }

    #[test]
    fn the_command_rows_are_the_device_bitmap() {
        let profile = BleProfile::load();
        assert_eq!(row_bitmap(profile), profile.controller.supported_commands);
        let unknown: Vec<u16> = profile
            .commands
            .iter()
            .filter(|c| c.answer == Answering::Unknown)
            .map(|c| c.opcode)
            .collect();
        assert_eq!(unknown, [0x1005, 0x2060]);
        let id = &profile.controller;
        assert_eq!(
            (id.accept_list_size, id.resolving_list_size, id.adv_sets),
            (12, 10, 6)
        );
        assert_eq!((id.tx_power_min, id.tx_power_max), (-24, 21));
        // Bit 12, LE Extended Advertising, is reported by the device.
        assert_eq!(id.le_features, 0x0801_F9FF);
    }

    #[test]
    fn every_table_row_is_answered_by_its_own_arm() {
        for row in &BleProfile::load().commands {
            let mut c = with_le_meta();
            let want_kind = if hci::is_async(row.opcode) {
                hci::event::COMMAND_STATUS
            } else {
                hci::event::COMMAND_COMPLETE
            };
            match row.answer {
                Answering::Modeled => {}
                Answering::Unmodeled => {
                    let (code, status, op, _) = ack(&send(&mut c, row.opcode, &[]));
                    assert_eq!(
                        (code, status, op),
                        (want_kind, status::UNSUPPORTED_FEATURE, row.opcode),
                        "{}",
                        row.name
                    );
                    continue;
                }
                Answering::Unknown => {
                    let (code, status, _, ret) = ack(&send(&mut c, row.opcode, &[]));
                    assert_eq!(
                        (code, status, ret, c.unknown_commands),
                        (
                            hci::event::COMMAND_COMPLETE,
                            status::UNKNOWN_COMMAND,
                            vec![],
                            1
                        ),
                        "{}",
                        row.name
                    );
                    continue;
                }
            }
            let len = (0..=255)
                .find(|n| param_len(row.opcode, &vec![0u8; *n]).is_some())
                .unwrap_or_else(|| panic!("{} has no parameter length", row.name));
            let events = send(&mut c, row.opcode, &vec![0u8; len]);
            if row.opcode == 0x0C35 {
                assert!(events.is_empty());
                continue;
            }
            let (code, status, op, _) = ack(&events);
            assert_eq!(op, row.opcode, "{}", row.name);
            assert_ne!(status, status::UNKNOWN_COMMAND, "{}", row.name);
            assert_eq!(code, want_kind, "{}", row.name);
            assert_eq!(c.unknown_commands, 0, "{}", row.name);
            let (_, status, _, _) = ack(&send(&mut c, row.opcode, &vec![0u8; len + 1]));
            assert_eq!(status, status::INVALID_PARAMETERS, "{}", row.name);
        }
    }

    #[test]
    fn unsupported_commands_get_command_status_or_complete_with_unknown_command() {
        let mut c = Controller::default();
        // HCI_Inquiry is asynchronous; HCI_Read_Local_Name synchronous; 0xFC11 vendor-specific.
        assert_eq!(
            ack(&send(&mut c, 0x0401, &[0; 5])),
            (
                hci::event::COMMAND_STATUS,
                status::UNKNOWN_COMMAND,
                0x0401,
                vec![]
            )
        );
        let (code, status, op, _) = ack(&send(&mut c, 0x0C14, &[]));
        assert_eq!(
            (code, status, op),
            (
                hci::event::COMMAND_COMPLETE,
                status::UNKNOWN_COMMAND,
                0x0C14
            )
        );
        let (code, status, op, _) = ack(&send(&mut c, 0xFC11, &[1, 2]));
        assert_eq!(
            (code, status, op),
            (hci::event::COMMAND_COMPLETE, 0, 0xFC11)
        );
        // Reported by the device and not modeled: Unsupported Feature or Parameter Value.
        let (code, status, _, _) = ack(&send(&mut c, 0x2039, &[0, 0]));
        assert_eq!(
            (code, status),
            (hci::event::COMMAND_COMPLETE, status::UNSUPPORTED_FEATURE)
        );
        let (code, status, _, _) = ack(&send(&mut c, 0x2043, &[0; 10]));
        assert_eq!(
            (code, status),
            (hci::event::COMMAND_STATUS, status::UNSUPPORTED_FEATURE)
        );
        assert_eq!(c.unknown_commands, 2);
        assert_eq!(c.commands, 5);
    }

    #[test]
    fn read_remote_version_and_connection_update_complete_after_their_status() {
        let mut c = with_le_meta();
        let events = send(&mut c, 0x041D, &[1, 0]);
        assert_eq!(events.len(), 2);
        assert_eq!(ack(&events).1, status::SUCCESS);
        assert_eq!(
            events[1],
            [
                0x04, 0x0C, 0x08, 0x00, 0x01, 0x00, 9, 0xE5, 0x02, 0x16, 0x00
            ]
        );
        let events = send(&mut c, 0x2013, &update_params(1));
        assert_eq!(events.len(), 2);
        assert_eq!(
            ack(&events),
            (hci::event::COMMAND_STATUS, 0, 0x2013, vec![])
        );
        assert_eq!(
            events[1],
            [
                0x04, 0x3E, 0x0A, 0x03, 0x00, 0x01, 0x00, 12, 0, 4, 0, 0x90, 0x01
            ]
        );
        assert_eq!(
            (c.connections[0].interval, c.connections[0].timeout),
            (12, 400)
        );

        for (op, p) in [(0x041D, vec![2, 0]), (0x2013, update_params(2))] {
            let events = send(&mut c, op, &p);
            assert_eq!(events.len(), 1);
            assert_eq!(ack(&events).1, status::UNKNOWN_CONNECTION);
        }
        let mut bad = update_params(1);
        bad[8..10].copy_from_slice(&10u16.to_le_bytes());
        assert_eq!(
            ack(&send(&mut c, 0x2013, &bad)).1,
            status::INVALID_PARAMETERS
        );

        send(&mut c, 0x0C01, &0u64.to_le_bytes());
        assert_eq!(send(&mut c, 0x041D, &[1, 0]).len(), 1);
        assert_eq!(send(&mut c, 0x2013, &update_params(1)).len(), 1);
    }

    #[test]
    fn host_acl_packets_wait_for_their_connection_event_on_their_connection_only() {
        let mut c = with_le_meta();
        let commands = c.commands;
        let packet = [0x02, 0x01, 0x20, 0x03, 0x00, 0xAA, 0xBB, 0xCC];
        assert_eq!(acl(&mut c, &packet), Events::new());
        assert_eq!(c.connections[0].tx, [packet.to_vec()]);
        assert_eq!(
            acl(&mut c, &[0x02, 0x05, 0x20, 0x01, 0x00, 0xAA]),
            Events::new()
        );
        assert_eq!(acl(&mut c, &[0x02, 0x01, 0x20, 0x09, 0x00]), Events::new());
        assert_eq!((c.acl_packets, c.acl_dropped), (1, 2));
        // Longer than 251 octets, and beyond the 12 buffers: discarded.
        let mut long = vec![0x02, 0x01, 0x20, 0xFC, 0x00];
        long.extend_from_slice(&[0u8; 252]);
        assert_eq!(acl(&mut c, &long), Events::new());
        for _ in 0..12 {
            acl(&mut c, &packet);
        }
        assert_eq!((c.acl_packets, c.acl_dropped), (12, 4));
        assert_eq!(c.take_transmit(1, 4).len(), 4);
        assert_eq!(c.connections[0].tx.len(), 8);
        assert!(c.take_transmit(2, 4).is_empty());
        // §7.3.40: silent when well formed or empty; a Num_Handles that disagrees with the length
        // gets a Command Complete.
        assert!(send(&mut c, 0x0C35, &[1, 1, 0, 1, 0]).is_empty());
        assert!(send(&mut c, 0x0C35, &[]).is_empty());
        let events = send(&mut c, 0x0C35, &[2, 1, 0, 1, 0]);
        assert_eq!(
            ack(&events),
            (
                hci::event::COMMAND_COMPLETE,
                status::INVALID_PARAMETERS,
                0x0C35,
                vec![]
            )
        );
        assert_eq!(c.commands, commands + 3);
    }

    #[test]
    fn privacy_commands_follow_the_resolving_list() {
        let mut c = Controller::default();
        let mut entry = vec![0u8; 39];
        entry[1..7].copy_from_slice(&ADDR);
        let mut mode = entry[..7].to_vec();
        mode.push(1);
        assert_eq!(
            ack(&send(&mut c, 0x204E, &mode)).1,
            status::UNKNOWN_CONNECTION
        );
        assert_eq!(ack(&send(&mut c, 0x2027, &entry)).1, status::SUCCESS);
        assert_eq!(
            ack(&send(&mut c, 0x2027, &entry)).1,
            status::INVALID_PARAMETERS
        );
        assert_eq!(ack(&send(&mut c, 0x204E, &mode)).1, status::SUCCESS);
        assert_eq!(c.resolving_list[0][7], 1);
        assert_eq!(ack(&send(&mut c, 0x202A, &[])).3, [10]);
        send(&mut c, 0x202D, &[1]);
        send(&mut c, 0x200A, &[1]);
        assert_eq!(
            ack(&send(&mut c, 0x2029, &[])).1,
            status::COMMAND_DISALLOWED
        );
        assert_eq!(
            ack(&send(&mut c, 0x204E, &mode)).1,
            status::COMMAND_DISALLOWED
        );
        send(&mut c, 0x200A, &[0]);
        assert_eq!(ack(&send(&mut c, 0x2028, &entry[..7])).1, status::SUCCESS);
        assert_eq!(
            ack(&send(&mut c, 0x2028, &entry[..7])).1,
            status::UNKNOWN_CONNECTION
        );
        assert_eq!(
            ack(&send(&mut c, 0x202E, &[0, 0])).1,
            status::INVALID_PARAMETERS
        );
        // §7.8.45: RPA_Timeout 0x0001 to 0x0E10 (1 s to 1 h).
        assert_eq!(ack(&send(&mut c, 0x202E, &[0x10, 0x0E])).1, status::SUCCESS);
        for high in [0x0E11u16, 0xA1B8] {
            assert_eq!(
                status_of(&mut c, 0x202E, &high.to_le_bytes()),
                status::INVALID_PARAMETERS
            );
        }
    }

    #[test]
    fn le_encrypt_is_fips_197_with_little_endian_parameters() {
        let mut c = Controller::default();
        let mut params = Vec::new();
        params.extend((0u8..16).rev());
        params.extend((0u8..16).rev().map(|i| i * 0x11));
        let (_, status, _, out) = ack(&send(&mut c, 0x2017, &params));
        assert_eq!(status, 0);
        let mut want = [
            0x69, 0xC4, 0xE0, 0xD8, 0x6A, 0x7B, 0x04, 0x30, 0xD8, 0xCD, 0xB7, 0x80, 0x70, 0xB4,
            0xC5, 0x5A,
        ];
        want.reverse();
        assert_eq!(out, want);
    }

    #[test]
    fn le_rand_follows_the_seed_stream() {
        let rand = |c: &mut Controller, rng: &mut DetRng| {
            let events = c.on_packet(ADDR, &cmd(0x2018, &[]), &mut |out| {
                rng.stream(RngStream::RADIO_BLE).fill_bytes(out)
            });
            ack(&events).3
        };
        let (mut one, mut again, mut two) = (DetRng::new(1), DetRng::new(1), DetRng::new(2));
        let mut c = Controller::default();
        let first = rand(&mut c, &mut one);
        assert_eq!(first.len(), 8);
        assert_eq!(first, rand(&mut c, &mut again));
        assert_ne!(first, rand(&mut c, &mut two));
        assert_eq!(one.pos_of(RngStream::RADIO_BLE), 2);
        assert_eq!(one.positions().count(), 1);
        // A generator restored at the same position continues identically, across HCI_Reset too.
        let mut restored = one.clone();
        send(&mut c, 0x0C03, &[]);
        let second = rand(&mut c, &mut one);
        assert_ne!(first, second);
        assert_eq!(second, rand(&mut Controller::default(), &mut restored));
    }

    #[test]
    fn advertising_checks_its_parameters_and_address() {
        let mut c = Controller::default();
        let mut data = vec![32];
        data.resize(32, 0);
        assert_eq!(
            ack(&send(&mut c, 0x2008, &data)).1,
            status::INVALID_PARAMETERS
        );
        let mut params = vec![0x20, 0, 0x20, 0, 0x03, 0x01, 0, 0, 0, 0, 0, 0, 0, 0x07, 0];
        assert_eq!(ack(&send(&mut c, 0x2006, &params)).1, status::SUCCESS);
        assert_eq!(
            ack(&send(&mut c, 0x200A, &[1])).1,
            status::INVALID_PARAMETERS
        );
        assert_eq!(ack(&send(&mut c, 0x2005, &[1, 2, 3, 4, 5, 0xC0])).1, 0);
        assert_eq!(ack(&send(&mut c, 0x200A, &[1])).1, 0);
        assert_eq!(
            ack(&send(&mut c, 0x2005, &[1, 2, 3, 4, 5, 0xC0])).1,
            status::COMMAND_DISALLOWED
        );
        params[0] = 0x10;
        send(&mut c, 0x200A, &[0]);
        assert_eq!(
            ack(&send(&mut c, 0x2006, &params)).1,
            status::INVALID_PARAMETERS
        );
        let commands = c.commands;
        send(&mut c, 0x0C03, &[]);
        assert_eq!(c.advertising, Advertising::default());
        assert_eq!(c.random_address, [0; 6]);
        assert_eq!(c.commands, commands + 1);
    }

    #[test]
    fn connection_procedures_report_their_completions() {
        let mut c = with_le_meta();
        let events = send(&mut c, 0x2022, &[1, 0, 0xFB, 0, 0x48, 0x08]);
        assert_eq!(ack(&events).3, [1, 0]);
        assert_eq!(
            events[1],
            [
                0x04, 0x3E, 0x0B, 0x07, 1, 0, 0xFB, 0, 0x48, 0x08, 0xFB, 0, 0x90, 0x42
            ]
        );
        assert_eq!(
            send(&mut c, 0x2022, &[1, 0, 0xFB, 0, 0x48, 0x08]).len(),
            1,
            "unchanged"
        );
        let events = send(&mut c, 0x2032, &[1, 0, 0, 2, 2, 0, 0]);
        assert_eq!(events[1], [0x04, 0x3E, 0x06, 0x0C, 0, 1, 0, 2, 2]);
        assert_eq!(ack(&send(&mut c, 0x2030, &[1, 0])).3, [1, 0, 2, 2]);
        let mut create = vec![0x60, 0, 0x30, 0, 0, 0, 1, 2, 3, 4, 5, 6, 0];
        for v in [24u16, 24, 0, 72, 0, 0] {
            create.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            ack(&send(&mut c, 0x200D, &create)).0,
            hci::event::COMMAND_STATUS
        );
        assert_eq!(
            ack(&send(&mut c, 0x200D, &create)).1,
            status::COMMAND_DISALLOWED
        );
        let events = send(&mut c, 0x200E, &[]);
        assert_eq!(ack(&events).1, 0);
        assert_eq!(
            events[1][..5],
            [0x04, 0x3E, 0x22, 0x29, status::UNKNOWN_CONNECTION]
        );
        assert_eq!(
            ack(&send(&mut c, 0x200E, &[])).1,
            status::COMMAND_DISALLOWED
        );
        assert_eq!(
            ack(&send(&mut c, 0x0406, &[1, 0, 0x01])).1,
            status::INVALID_PARAMETERS
        );
        let events = send(&mut c, 0x0406, &[1, 0, 0x13]);
        assert_eq!(events.len(), 1, "no Disconnection Complete yet");
        assert_eq!(c.terminating(1), Some(0x13));
        assert_eq!(
            ack(&send(&mut c, 0x0406, &[1, 0, 0x13])).1,
            status::COMMAND_DISALLOWED,
            "the procedure is already running"
        );
        assert_eq!(c.connections.len(), 1);
        assert_eq!(
            c.complete_disconnect(1),
            Some(hci::event(
                hci::event::DISCONNECTION_COMPLETE,
                &[0, 1, 0, status::LOCAL_HOST_TERMINATED]
            ))
        );
        assert!(c.connections.is_empty());
    }

    fn create_connection(filter: u8) -> Vec<u8> {
        let mut create = vec![0x60, 0, 0x30, 0, filter, 0, 1, 2, 3, 4, 5, 6, 0];
        for v in [24u16, 24, 0, 72, 0, 0] {
            create.extend_from_slice(&v.to_le_bytes());
        }
        create
    }

    /// LE Meta (`Event_Mask` bit 61) is outside the default mask (§7.3.1), so it is set first, as
    /// the host does.
    #[test]
    fn a_cancelled_connection_completes_by_the_le_event_mask() {
        let mut c = Controller::default();
        send(&mut c, 0x0C01, &(mask::DEFAULT | 1 << 61).to_le_bytes());
        send(&mut c, 0x2001, &0x3FFu64.to_le_bytes());
        assert_eq!(status_of(&mut c, 0x200D, &create_connection(0)), 0);
        let events = send(&mut c, 0x200E, &[]);
        assert_eq!(events.len(), 2);
        let mut want = vec![0x04, 0x3E, 0x1F, 0x0A, status::UNKNOWN_CONNECTION];
        want.resize(34, 0);
        assert_eq!(events[1], want);

        send(&mut c, 0x2001, &0x1FFu64.to_le_bytes());
        send(&mut c, 0x200D, &create_connection(0));
        let events = send(&mut c, 0x200E, &[]);
        let mut want = vec![0x04, 0x3E, 0x13, 0x01, status::UNKNOWN_CONNECTION];
        want.resize(22, 0);
        assert_eq!(events[1], want);

        send(&mut c, 0x2001, &0u64.to_le_bytes());
        send(&mut c, 0x200D, &create_connection(0));
        assert_eq!(send(&mut c, 0x200E, &[]).len(), 1);
    }

    #[test]
    fn encryption_change_follows_page_2_and_ltk_replies_need_a_request() {
        let mut c = with_le_meta();
        let mut start = vec![1, 0];
        start.extend_from_slice(&[0; 26]);
        let events = send(&mut c, 0x2019, &start);
        assert_eq!(events[1], [0x04, 0x08, 0x04, 0, 1, 0, 0x01]);
        send(&mut c, 0x0C63, &(1u64 << 25).to_le_bytes());
        let events = send(&mut c, 0x2019, &start);
        assert_eq!(events[1], [0x04, 0x59, 0x05, 0, 1, 0, 0x01, 16]);

        let mut reply = vec![1, 0];
        reply.extend_from_slice(&[0x11; 16]);
        let (_, code, _, ret) = ack(&send(&mut c, 0x201A, &reply));
        assert_eq!((code, ret), (status::COMMAND_DISALLOWED, vec![1, 0]));
        assert_eq!(
            status_of(&mut c, 0x201B, &[1, 0]),
            status::COMMAND_DISALLOWED
        );
        let request = c.request_ltk(1, [0; 8], 0).expect("LE Meta is enabled");
        assert_eq!(request[..4], [0x04, 0x3E, 0x0D, 0x05]);
        let events = send(&mut c, 0x201A, &reply);
        assert_eq!(ack(&events).1, status::SUCCESS);
        assert_eq!(events[1], [0x04, 0x59, 0x05, 0, 1, 0, 0x01, 16]);
        assert_eq!(
            status_of(&mut c, 0x201A, &reply),
            status::COMMAND_DISALLOWED
        );
        assert_eq!(
            status_of(
                &mut c,
                0x201A,
                &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
            ),
            status::UNKNOWN_CONNECTION
        );
    }

    #[test]
    fn the_accept_list_is_locked_while_a_filter_policy_uses_it() {
        let entry = [0, 1, 2, 3, 4, 5, 6];
        let check = |c: &mut Controller, want: u8| {
            assert_eq!(status_of(c, 0x2011, &entry), want, "add");
            assert_eq!(status_of(c, 0x2012, &entry), want, "remove");
            assert_eq!(status_of(c, 0x2010, &[]), want, "clear");
        };
        let mut c = Controller::default();
        let mut adv = vec![0x20, 0, 0x20, 0, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0];
        send(&mut c, 0x2006, &adv);
        send(&mut c, 0x200A, &[1]);
        check(&mut c, status::SUCCESS);
        send(&mut c, 0x200A, &[0]);
        adv[14] = 1;
        send(&mut c, 0x2006, &adv);
        send(&mut c, 0x200A, &[1]);
        check(&mut c, status::COMMAND_DISALLOWED);
        send(&mut c, 0x200A, &[0]);
        check(&mut c, status::SUCCESS);

        send(&mut c, 0x200B, &[0, 0x10, 0, 0x10, 0, 0, 1]);
        send(&mut c, 0x200C, &[1, 0]);
        check(&mut c, status::COMMAND_DISALLOWED);
        send(&mut c, 0x200C, &[0, 0]);

        send(&mut c, 0x200D, &create_connection(1));
        check(&mut c, status::COMMAND_DISALLOWED);
        send(&mut c, 0x200E, &[]);
        send(&mut c, 0x200D, &create_connection(0));
        check(&mut c, status::SUCCESS);
    }

    #[test]
    fn set_default_phy_checks_its_preferences() {
        let mut c = Controller::default();
        for (params, want) in [
            ([0x00, 0x01, 0x01], status::SUCCESS),
            ([0x00, 0x03, 0x03], status::SUCCESS),
            ([0x00, 0x04, 0x04], status::SUCCESS),
            ([0x03, 0x00, 0x00], status::SUCCESS),
            ([0x01, 0x00, 0x02], status::SUCCESS),
            ([0x04, 0x01, 0x01], status::INVALID_PARAMETERS),
            ([0x00, 0x00, 0x01], status::INVALID_PARAMETERS),
            ([0x02, 0x01, 0x00], status::SUCCESS),
            ([0x01, 0x01, 0x00], status::INVALID_PARAMETERS),
            ([0x00, 0x08, 0x08], status::UNSUPPORTED_FEATURE),
            ([0x00, 0x01, 0x02], status::UNSUPPORTED_FEATURE),
        ] {
            assert_eq!(status_of(&mut c, 0x2031, &params), want, "{params:02x?}");
        }
        assert_eq!(c.default_phy, [0x02, 0x01, 0x00]);
    }

    #[test]
    fn legacy_and_extended_advertising_commands_do_not_mix() {
        let mut c = Controller::default();
        assert_eq!(ack(&send(&mut c, 0x203A, &[])).3, [0x72, 0x06]);
        assert_eq!(
            ack(&send(&mut c, 0x2007, &[])),
            (
                hci::event::COMMAND_COMPLETE,
                status::COMMAND_DISALLOWED,
                0x2007,
                vec![9]
            )
        );
        let adv = [0x20, 0, 0x20, 0, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0x07, 0];
        assert_eq!(status_of(&mut c, 0x2006, &adv), status::COMMAND_DISALLOWED);
        let (code, status, _, _) = ack(&send(&mut c, 0x200D, &create_connection(0)));
        assert_eq!(
            (code, status),
            (hci::event::COMMAND_STATUS, status::COMMAND_DISALLOWED)
        );
        assert!(!c.initiating);
        assert_eq!(status_of(&mut c, 0x2003, &[]), status::SUCCESS);

        send(&mut c, 0x0C03, &[]);
        assert_eq!(ack(&send(&mut c, 0x2007, &[])).3, [9]);
        assert_eq!(status_of(&mut c, 0x2006, &adv), status::SUCCESS);
        assert_eq!(status_of(&mut c, 0x203B, &[]), status::COMMAND_DISALLOWED);
        assert_eq!(
            status_of(&mut c, 0x2039, &[0, 0]),
            status::COMMAND_DISALLOWED
        );
        let (code, status, _, _) = ack(&send(&mut c, 0x2043, &[0; 10]));
        assert_eq!(
            (code, status),
            (hci::event::COMMAND_STATUS, status::COMMAND_DISALLOWED)
        );
        assert_eq!(c.advertising_family, LEGACY);
    }

    #[test]
    fn power_timeout_test_mode_and_rf_path_commands_follow_the_specification() {
        let mut c = with_le_meta();
        assert_eq!(ack(&send(&mut c, 0x0C2D, &[1, 0, 0])).3, [1, 0, 9]);
        assert_eq!(ack(&send(&mut c, 0x0C2D, &[1, 0, 1])).3, [1, 0, 20]);
        assert_eq!(
            status_of(&mut c, 0x0C2D, &[1, 0, 2]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(
            status_of(&mut c, 0x0C2D, &[2, 0, 0]),
            status::UNKNOWN_CONNECTION
        );

        // 30 s by default; the floor on interval 24 and latency 0 is 3 (30 ms).
        assert_eq!(ack(&send(&mut c, 0x0C7B, &[1, 0])).3, [1, 0, 0xB8, 0x0B]);
        assert_eq!(
            status_of(&mut c, 0x0C7C, &[1, 0, 2, 0]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(
            status_of(&mut c, 0x0C7C, &[1, 0, 0, 0]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(status_of(&mut c, 0x0C7C, &[1, 0, 3, 0]), status::SUCCESS);
        assert_eq!(ack(&send(&mut c, 0x0C7B, &[1, 0])).3, [1, 0, 3, 0]);
        assert_eq!(
            status_of(&mut c, 0x0C7B, &[2, 0]),
            status::UNKNOWN_CONNECTION
        );

        assert_eq!(status_of(&mut c, 0x201D, &[0]), status::COMMAND_DISALLOWED);
        let mut c = Controller::default();
        assert_eq!(status_of(&mut c, 0x201F, &[]), status::COMMAND_DISALLOWED);
        assert_eq!(
            status_of(&mut c, 0x201D, &[0x28]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(
            status_of(&mut c, 0x2033, &[0, 5, 0]),
            status::UNSUPPORTED_FEATURE
        );
        assert_eq!(
            status_of(&mut c, 0x2033, &[0, 3, 2]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(status_of(&mut c, 0x2033, &[39, 3, 1]), status::SUCCESS);
        assert_eq!(c.test, RECEIVER_TEST);
        assert_eq!(
            status_of(&mut c, 0x201E, &[0, 37, 0]),
            status::COMMAND_DISALLOWED
        );
        assert_eq!(ack(&send(&mut c, 0x201F, &[])).3, [0, 0]);
        assert_eq!(
            status_of(&mut c, 0x2034, &[0, 37, 8, 1]),
            status::INVALID_PARAMETERS
        );
        assert_eq!(status_of(&mut c, 0x2034, &[0, 37, 4, 4]), status::SUCCESS);
        assert_eq!(c.test, TRANSMITTER_TEST);
        send(&mut c, 0x201F, &[]);
        assert_eq!(c.test, 0);

        assert_eq!(ack(&send(&mut c, 0x204C, &[])).3, [0, 0, 0, 0]);
        let (tx, rx) = ((-1280i16) as u16, 1280u16);
        let mut params = tx.to_le_bytes().to_vec();
        params.extend_from_slice(&rx.to_le_bytes());
        assert_eq!(status_of(&mut c, 0x204D, &params), status::SUCCESS);
        assert_eq!(ack(&send(&mut c, 0x204C, &[])).3, [0x00, 0xFB, 0x00, 0x05]);
        params[2..].copy_from_slice(&1281u16.to_le_bytes());
        assert_eq!(
            status_of(&mut c, 0x204D, &params),
            status::INVALID_PARAMETERS
        );
    }

    #[test]
    fn the_controller_state_round_trips_through_its_snapshot_codec() {
        let mut c = with_le_meta();
        send(&mut c, 0x2018, &[]);
        send(&mut c, 0x2013, &update_params(1));
        let mut bytes = Vec::new();
        c.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "controller");
        assert_eq!(Controller::snap_read(&mut r), Ok(c));
        assert!(r.is_empty());
    }
}
