//! The virtual air between the emulated controller and the scripted central, written from the
//! Bluetooth Core Specification 5.4 Vol 6 Part B (link layer).
//!
//! - Advertising events (§4.4.2.2): one every `advInterval` plus an `advDelay` of 0 to 10 ms from
//!   `RngStream::RADIO_BLE`, on channels 37, 38, 39 of `Advertising_Channel_Map`. High duty cycle
//!   directed advertising runs every 3.75 ms with no delay and ends after 1.28 s.
//! - A `SCAN_REQ` gets a `SCAN_RSP` and a `CONNECT_IND` ends advertising, both `T_IFS` after the
//!   PDU, subject to `Advertising_Filter_Policy` and the Filter Accept List.
//! - Connection events (§4.5.1): the first anchor is `transmitWindowDelay` after the
//!   `CONNECT_IND`, then one per `connInterval`.
//! - Ending a link (§5.1.3): a host `HCI_Disconnect` puts `LL_TERMINATE_IND` on the air at the
//!   next event and completes at the event after it. Packets still buffered get no
//!   `HCI_Number_Of_Completed_Packets` (§7.7.19 reports live connections only).
//!
//! Not modeled: peripheral latency, supervision timeouts, and CSA#2. The PDUs leave `ChSel` clear,
//! so the link uses algorithm #1 although the device's `LE_Features` sets bit 6.
//!
//! All PDUs of one event share its instant except `T_IFS` between a PDU and its reply; the gaps
//! inside a connection event are below the resolution the tests check (class C). A command reply
//! is due `reply_us` after the host wrote it.

use super::central::{self, AdvAction, Central, LlRequest};
use super::controller::Controller;
use super::hci;
use pemu_core::snap::snap_struct;

/// Link layer PDU types (Core Vol 6 Part B §2.3).
pub mod pdu {
    /// `ADV_IND`, connectable and scannable undirected.
    pub const ADV_IND: u8 = 0b0000;
    /// `ADV_DIRECT_IND`, connectable directed.
    pub const ADV_DIRECT_IND: u8 = 0b0001;
    /// `ADV_NONCONN_IND`, non-connectable and non-scannable undirected.
    pub const ADV_NONCONN_IND: u8 = 0b0010;
    pub const SCAN_REQ: u8 = 0b0011;
    pub const SCAN_RSP: u8 = 0b0100;
    pub const CONNECT_IND: u8 = 0b0101;
    /// `ADV_SCAN_IND`, scannable undirected.
    pub const ADV_SCAN_IND: u8 = 0b0110;
}

/// Module timer tags.
pub const TIMER_ADVERTISING: u16 = 2;
pub const TIMER_CONNECTION: u16 = 3;
pub const TIMER_CENTRAL: u16 = 4;

/// The advertising physical channels in the order an event uses them (§4.4.2.1).
pub const ADV_CHANNELS: [u8; 3] = [37, 38, 39];
/// §4.4.2.2.1.
pub const ADV_DELAY_MAX_US: u64 = 10_000;
/// Class C: §4.4.2.2 bounds only the whole event (at most 10 ms between PDU starts).
pub const CHANNEL_SPACING_US: u64 = 1_500;
/// The largest interval §4.4.2.4.2 allows.
pub const HIGH_DUTY_INTERVAL_US: u64 = 3_750;
/// Vol 4 Part E §7.8.9.
pub const HIGH_DUTY_TIMEOUT_US: u64 = 1_280_000;
/// §4.1.1.
pub const T_IFS_US: u64 = 150;
/// On the LE 1M PHY (§4.5.3).
pub const TRANSMIT_WINDOW_DELAY_US: u64 = 1_250;
/// Data PDUs per direction per connection event, class C.
pub const PDUS_PER_EVENT: usize = 4;
/// The default `connMaxTxOctets` (§4.5.10).
pub const CENTRAL_TX_OCTETS: usize = 27;
pub const AIR_LOG: usize = 64;

/// One PDU on the air: the 2-octet header and the payload (Core Vol 6 Part B §2.3, §2.4), with
/// no preamble, access address or CRC.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AirRecord {
    pub at_us: u64,
    pub channel: u8,
    pub from_central: bool,
    pub pdu: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    pub handle: u16,
    pub events: u64,
    /// `hopIncrement` of channel selection algorithm #1 (§4.5.8.2), 5 to 16.
    pub hop: u8,
    /// `lastUnmappedChannel`.
    pub last_channel: u8,
    pub to_peripheral: Vec<Fragment>,
    pub to_central: Vec<Fragment>,
    /// `LL_TERMINATE_IND` sent, waiting for the acknowledgement (§5.1.3).
    pub terminate_sent: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fragment {
    /// `LLID` 0b10, the start of an L2CAP frame (else 0b01, a continuation).
    pub start: bool,
    pub data: Vec<u8>,
    /// The last fragment of its host packet: `HCI_Number_Of_Completed_Packets` reports the packet
    /// once this goes out.
    pub last: bool,
}

snap_struct!(Fragment { start, data, last });

/// The air state. Each event is a module timer, and the armed instants are recorded so a stale
/// timer does nothing and a snapshot between two events restores the same schedule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Air {
    pub adv_timer_us: Option<u64>,
    /// For the high duty cycle timeout.
    pub adv_since_us: u64,
    pub adv_events: u64,
    pub conn_timer_us: Option<u64>,
    pub central_timer_us: Option<u64>,
    pub link: Option<Link>,
    pub log: Vec<AirRecord>,
    pub pdus: u64,
}

snap_struct!(AirRecord {
    at_us,
    channel,
    from_central,
    pdu
});
snap_struct!(Link {
    handle,
    events,
    hop,
    last_channel,
    to_peripheral,
    to_central,
    terminate_sent
});
snap_struct!(Air {
    adv_timer_us,
    adv_since_us,
    adv_events,
    conn_timer_us,
    central_timer_us,
    link,
    log,
    pdus
});

/// What an air step asks of the module: host events with their due instant, and module timers
/// (instant, tag).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AirOut {
    pub events: Vec<(u64, Vec<u8>)>,
    pub timers: Vec<(u64, u16)>,
}

/// The machine seed's `RngStream::RADIO_BLE` in a machine.
pub type Entropy<'a> = &'a mut dyn FnMut(&mut [u8]);

/// One legacy advertising PDU (Core Vol 6 Part B §2.3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdvertisingPdu {
    pub pdu_type: u8,
    /// `TxAdd`.
    pub random_address: bool,
    /// Least significant octet first.
    pub adv_a: [u8; 6],
    /// Empty for `ADV_DIRECT_IND`, which carries `TargetA` instead.
    pub adv_data: Vec<u8>,
}

impl AdvertisingPdu {
    pub fn from_air(record: &AirRecord) -> Option<AdvertisingPdu> {
        let (&h0, rest) = record.pdu.split_first()?;
        let (_, payload) = rest.split_first()?;
        let pdu_type = h0 & 0x0F;
        if record.from_central
            || !matches!(
                pdu_type,
                pdu::ADV_IND | pdu::ADV_DIRECT_IND | pdu::ADV_NONCONN_IND | pdu::ADV_SCAN_IND
            )
        {
            return None;
        }
        Some(AdvertisingPdu {
            pdu_type,
            random_address: h0 & 0x40 != 0,
            adv_a: payload.get(..6)?.try_into().ok()?,
            adv_data: if pdu_type == pdu::ADV_DIRECT_IND {
                Vec::new()
            } else {
                payload[6..].to_vec()
            },
        })
    }
}

/// The PDU legacy advertising puts on the air, or `None` while it is disabled.
///
/// `Advertising_Type` maps by Core Vol 4 Part E §7.8.5: 0 `ADV_IND`, 1 and 4 `ADV_DIRECT_IND`,
/// 2 `ADV_SCAN_IND`, 3 `ADV_NONCONN_IND`. `Own_Address_Type` 2 and 3 fall back to the public and
/// random identity, because this controller generates no resolvable private address.
pub fn advertising_pdu(controller: &Controller, public_address: [u8; 6]) -> Option<AdvertisingPdu> {
    let adv = &controller.advertising;
    if !adv.enabled {
        return None;
    }
    let pdu_type = match adv.kind() {
        0 => pdu::ADV_IND,
        1 | 4 => pdu::ADV_DIRECT_IND,
        2 => pdu::ADV_SCAN_IND,
        _ => pdu::ADV_NONCONN_IND,
    };
    let random = matches!(adv.own_address_type(), 1 | 3);
    Some(AdvertisingPdu {
        pdu_type,
        random_address: random,
        adv_a: if random {
            controller.random_address
        } else {
            public_address
        },
        adv_data: if pdu_type == pdu::ADV_DIRECT_IND {
            Vec::new()
        } else {
            adv.data.clone()
        },
    })
}

fn frame(h0: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![h0, payload.len() as u8];
    out.extend_from_slice(payload);
    out
}

/// Air time on LE 1M: preamble 1, access address 4, header 2, payload, CRC 3, at 8 us per bit
/// (§2.1).
pub fn pdu_air_us(len: usize) -> u64 {
    (10 + len as u64) * 8
}

fn draw_u16(entropy: Entropy<'_>) -> u16 {
    let mut b = [0u8; 2];
    entropy(&mut b);
    u16::from_le_bytes(b)
}

impl Air {
    fn log(&mut self, at_us: u64, channel: u8, from_central: bool, pdu: Vec<u8>) {
        self.pdus += 1;
        self.log.push(AirRecord {
            at_us,
            channel,
            from_central,
            pdu,
        });
        if self.log.len() > AIR_LOG {
            self.log.remove(0);
        }
    }

    pub fn last_advertising(&self) -> Option<(&AirRecord, AdvertisingPdu)> {
        self.log
            .iter()
            .rev()
            .find_map(|r| AdvertisingPdu::from_air(r).map(|p| (r, p)))
    }

    /// Arms the timers the current state needs. Called after every host packet, input and timer.
    pub fn sync(
        &mut self,
        c: &mut Controller,
        central: &Central,
        now_us: u64,
        entropy: Entropy<'_>,
        out: &mut AirOut,
    ) {
        if c.advertising.enabled && self.adv_timer_us.is_none() {
            self.adv_since_us = now_us;
            let at = now_us + self.adv_delay(c, entropy);
            self.adv_timer_us = Some(at);
            out.timers.push((at, TIMER_ADVERTISING));
        } else if !c.advertising.enabled {
            self.adv_timer_us = None;
        }
        let want = central.timer_us().map(|t| t.max(now_us));
        if want.is_some() && want != self.central_timer_us {
            out.timers.push((want.unwrap_or(now_us), TIMER_CENTRAL));
        }
        self.central_timer_us = want;
    }

    fn adv_delay(&self, c: &Controller, entropy: Entropy<'_>) -> u64 {
        if c.advertising.kind() == 1 {
            0
        } else {
            u64::from(draw_u16(entropy)) % (ADV_DELAY_MAX_US + 1)
        }
    }

    pub fn on_advertising_timer(
        &mut self,
        c: &mut Controller,
        central: &mut Central,
        public_address: [u8; 6],
        now_us: u64,
        entropy: Entropy<'_>,
    ) -> AirOut {
        let mut out = AirOut::default();
        if self.adv_timer_us != Some(now_us) {
            return out;
        }
        self.adv_timer_us = None;
        if !c.advertising.enabled {
            return out;
        }
        let adv = c.advertising.clone();
        let kind = adv.kind();
        // §4.4.2.4.3: the timeout is decided before the event, so none starts past the deadline.
        if kind == 1 && now_us.saturating_sub(self.adv_since_us) >= HIGH_DUTY_TIMEOUT_US {
            if let Some(ev) = c.advertising_timeout() {
                out.events.push((now_us, ev));
            }
            return out;
        }
        self.adv_events += 1;
        let pdu_type = match kind {
            0 => pdu::ADV_IND,
            1 | 4 => pdu::ADV_DIRECT_IND,
            2 => pdu::ADV_SCAN_IND,
            _ => pdu::ADV_NONCONN_IND,
        };
        let random = matches!(adv.own_address_type(), 1 | 3);
        let adv_a = if random {
            c.random_address
        } else {
            public_address
        };
        let target_random = adv.params[6] == 1;
        let target: [u8; 6] = adv.params[7..13].try_into().unwrap_or_default();
        let mut payload = adv_a.to_vec();
        let mut h0 = pdu_type | if random { 0x40 } else { 0 };
        if pdu_type == pdu::ADV_DIRECT_IND {
            payload.extend_from_slice(&target);
            h0 |= if target_random { 0x80 } else { 0 };
        } else {
            payload.extend_from_slice(&adv.data);
        }
        let to_central =
            pdu_type == pdu::ADV_DIRECT_IND && !target_random && target == central::CENTRAL_ADDRESS;
        // §4.3.2: filter policy 1 and 3 answer only accept-listed scanners, 2 and 3 only
        // accept-listed initiators. Directed advertising ignores the policy (Vol 4 Part E §7.8.5);
        // only `TargetA` decides.
        let mut central_entry = [0u8; 7];
        central_entry[1..].copy_from_slice(&central::CENTRAL_ADDRESS);
        let listed = c.accept_list.contains(&central_entry);
        let policy = if pdu_type == pdu::ADV_DIRECT_IND {
            0
        } else {
            adv.params[14]
        };
        let map = adv.params[13];
        let channels = ADV_CHANNELS
            .iter()
            .enumerate()
            .filter(|(i, _)| map & (1 << i) != 0)
            .map(|(_, ch)| *ch);
        for (i, channel) in channels.enumerate() {
            let at = now_us + i as u64 * CHANNEL_SPACING_US;
            let pdu = frame(h0, &payload);
            let reply_at = at + pdu_air_us(payload.len()) + T_IFS_US;
            self.log(at, channel, false, pdu);
            let data = if pdu_type == pdu::ADV_DIRECT_IND {
                &[][..]
            } else {
                &adv.data[..]
            };
            match central.on_advertising(at, channel, pdu_type, adv_a, random, data, to_central) {
                AdvAction::ScanRequest
                    if matches!(pdu_type, pdu::ADV_IND | pdu::ADV_SCAN_IND)
                        && (policy & 1 == 0 || listed) =>
                {
                    let mut req = central::CENTRAL_ADDRESS.to_vec();
                    req.extend_from_slice(&adv_a);
                    let rsp_at = reply_at + pdu_air_us(req.len()) + T_IFS_US;
                    let req = frame(pdu::SCAN_REQ | if random { 0x80 } else { 0 }, &req);
                    self.log(reply_at, channel, true, req);
                    let mut rsp = adv_a.to_vec();
                    rsp.extend_from_slice(&adv.scan_response);
                    let h = pdu::SCAN_RSP | if random { 0x40 } else { 0 };
                    self.log(rsp_at, channel, false, frame(h, &rsp));
                    central.on_scan_response(rsp_at, adv_a, random, &adv.scan_response);
                }
                AdvAction::Connect {
                    interval,
                    latency,
                    timeout,
                } if (policy & 2 == 0 || listed) => {
                    self.connect(
                        c, central, adv_a, random, reply_at, interval, latency, timeout, entropy,
                        &mut out,
                    );
                    return out;
                }
                _ => {}
            }
        }
        let interval_us = if kind == 1 {
            HIGH_DUTY_INTERVAL_US
        } else {
            // Class C: always `Advertising_Interval_Min`; which value the device picks in
            // [min, max] is UNVERIFIED.
            u64::from(adv.interval().0) * 625
        };
        let at = now_us + interval_us + self.adv_delay(c, entropy);
        self.adv_timer_us = Some(at);
        out.timers.push((at, TIMER_ADVERTISING));
        self.sync(c, central, now_us, entropy, &mut out);
        out
    }

    /// The central's `CONNECT_IND` (§2.3.3.1): `AA` and `CRCInit` from `entropy`, `WinSize` 1,
    /// `WinOffset` 0, all 37 data channels, `Hop` 5 to 16.
    #[allow(clippy::too_many_arguments)]
    fn connect(
        &mut self,
        c: &mut Controller,
        central: &mut Central,
        adv_a: [u8; 6],
        adv_random: bool,
        at_us: u64,
        interval: u16,
        latency: u16,
        timeout: u16,
        entropy: Entropy<'_>,
        out: &mut AirOut,
    ) {
        let mut ll = [0u8; 8];
        entropy(&mut ll);
        let hop = 5 + ll[7] % 12;
        let mut payload = central::CENTRAL_ADDRESS.to_vec();
        payload.extend_from_slice(&adv_a);
        payload.extend_from_slice(&ll[..7]);
        payload.push(1);
        payload.extend_from_slice(&[0, 0]);
        for v in [interval, latency, timeout] {
            payload.extend_from_slice(&v.to_le_bytes());
        }
        payload.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x1F]);
        payload.push(hop | (central::CENTRAL_SCA << 5));
        let h0 = pdu::CONNECT_IND | if adv_random { 0x80 } else { 0 };
        let channel = self.log.last().map_or(ADV_CHANNELS[0], |r| r.channel);
        self.log(at_us, channel, true, frame(h0, &payload));
        let handle = c.free_handle();
        if let Some(ev) = c.accept_connection(
            handle,
            false,
            central::CENTRAL_ADDRESS,
            interval,
            latency,
            timeout,
            central::CENTRAL_SCA,
        ) {
            out.events.push((at_us, ev));
        }
        self.adv_timer_us = None;
        self.link = Some(Link {
            handle,
            events: 0,
            hop,
            last_channel: 0,
            to_peripheral: Vec::new(),
            to_central: Vec::new(),
            terminate_sent: false,
        });
        let anchor = at_us + pdu_air_us(payload.len()) + TRANSMIT_WINDOW_DELAY_US;
        self.conn_timer_us = Some(anchor);
        out.timers.push((anchor, TIMER_CONNECTION));
        central.on_connected(handle, at_us);
        self.sync(c, central, at_us, entropy, out);
    }

    /// A connection event. The central transmits first, then the peripheral, at most
    /// [`PDUS_PER_EVENT`] data PDUs each way. A host ACL packet goes out at the first event after
    /// the host sent it. Both directions fragment to 27 octets, since no data length update runs
    /// (§4.5.10).
    pub fn on_connection_timer(
        &mut self,
        c: &mut Controller,
        central: &mut Central,
        now_us: u64,
        entropy: Entropy<'_>,
    ) -> AirOut {
        let mut out = AirOut::default();
        if self.conn_timer_us != Some(now_us) {
            return out;
        }
        self.conn_timer_us = None;
        let Some(mut link) = self.link.take() else {
            return out;
        };
        let handle = link.handle;
        if !c.has_connection(handle) {
            central.on_disconnected(now_us);
            self.sync(c, central, now_us, entropy, &mut out);
            return out;
        }
        link.events += 1;
        // CSA#1 (§4.5.8.2): all 37 data channels are mapped, so the remapping table is the
        // identity.
        link.last_channel = (link.last_channel + link.hop) % 37;
        let channel = link.last_channel;
        // A host `HCI_Disconnect` (§5.1.3), symmetric with the peer-initiated path below.
        if let Some(reason) = c.terminating(handle) {
            if link.terminate_sent {
                // The central's acknowledgement: an empty data PDU (§2.4).
                self.log(now_us, channel, true, frame(0b01, &[]));
                c.drop_in_flight(link.to_central.iter().filter(|f| f.last).count() as u16);
                if let Some(ev) = c.complete_disconnect(handle) {
                    out.events.push((now_us, ev));
                }
                self.sync(c, central, now_us, entropy, &mut out);
                return out;
            }
            // LL_TERMINATE_IND control PDU (§2.4.2.3).
            self.log(now_us, channel, false, frame(0b11, &[0x02, reason]));
            link.terminate_sent = true;
            central.on_disconnected(now_us);
            let next = now_us + u64::from(self.interval(c, handle)) * 1_250;
            self.link = Some(link);
            self.conn_timer_us = Some(next);
            out.timers.push((next, TIMER_CONNECTION));
            self.sync(c, central, now_us, entropy, &mut out);
            return out;
        }
        for request in central.take_ll_requests() {
            match request {
                LlRequest::Update {
                    interval,
                    latency,
                    timeout,
                } => {
                    if let Some(ev) = c.peer_update(handle, interval, latency, timeout) {
                        out.events.push((now_us, ev));
                    }
                }
                LlRequest::Terminate { reason } => {
                    // LL_TERMINATE_IND control PDU (§2.4.2.3).
                    self.log(now_us, channel, true, frame(0b11, &[0x02, reason]));
                    c.drop_in_flight(link.to_central.iter().filter(|f| f.last).count() as u16);
                    if let Some(ev) = c.remote_disconnect(handle, reason) {
                        out.events.push((now_us, ev));
                    }
                    central.on_disconnected(now_us);
                    self.sync(c, central, now_us, entropy, &mut out);
                    return out;
                }
            }
        }
        for l2cap in central.take_frames() {
            let chunks: Vec<&[u8]> = l2cap.chunks(CENTRAL_TX_OCTETS).collect();
            let n = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                link.to_peripheral.push(Fragment {
                    start: i == 0,
                    data: chunk.to_vec(),
                    last: i + 1 == n,
                });
            }
        }
        let n = link.to_peripheral.len().min(PDUS_PER_EVENT);
        for fragment in link.to_peripheral.drain(..n).collect::<Vec<_>>() {
            let (llid, pb) = if fragment.start {
                (0b10, hci::PB_FIRST_FLUSHABLE)
            } else {
                (0b01, hci::PB_CONTINUING)
            };
            self.log(now_us, channel, true, frame(llid, &fragment.data));
            out.events
                .push((now_us, hci::acl(handle, pb, &fragment.data)));
        }
        while link.to_central.len() < PDUS_PER_EVENT {
            let Some(packet) = c.take_transmit(handle, 1).into_iter().next() else {
                break;
            };
            let Some((_, pb, data)) = hci::acl_parts(&packet) else {
                continue;
            };
            let start = pb != hci::PB_CONTINUING;
            let chunks: Vec<&[u8]> = if data.is_empty() {
                vec![&[][..]]
            } else {
                data.chunks(CENTRAL_TX_OCTETS).collect()
            };
            let n = chunks.len();
            for (i, chunk) in chunks.into_iter().enumerate() {
                link.to_central.push(Fragment {
                    start: start && i == 0,
                    data: chunk.to_vec(),
                    last: i + 1 == n,
                });
            }
        }
        let n = link.to_central.len().min(PDUS_PER_EVENT);
        let mut completed = 0;
        for fragment in link.to_central.drain(..n).collect::<Vec<_>>() {
            let llid = if fragment.start { 0b10 } else { 0b01 };
            self.log(now_us, channel, false, frame(llid, &fragment.data));
            central.on_fragment(fragment.start, &fragment.data, now_us);
            completed += u16::from(fragment.last);
        }
        if let Some(ev) = c.completed(handle, completed) {
            out.events.push((now_us, ev));
        }
        let next = now_us + u64::from(self.interval(c, handle)) * 1_250;
        self.link = Some(link);
        self.conn_timer_us = Some(next);
        out.timers.push((next, TIMER_CONNECTION));
        self.sync(c, central, now_us, entropy, &mut out);
        out
    }

    /// In 1.25 ms units, at least the §4.5.1 minimum of 6.
    fn interval(&self, c: &Controller, handle: u16) -> u16 {
        c.connections
            .iter()
            .find(|conn| conn.handle == handle)
            .map_or(6, |conn| conn.interval)
            .max(6)
    }

    pub fn on_central_timer(
        &mut self,
        c: &mut Controller,
        central: &mut Central,
        now_us: u64,
        entropy: Entropy<'_>,
    ) -> AirOut {
        let mut out = AirOut::default();
        if self.central_timer_us != Some(now_us) {
            return out;
        }
        self.central_timer_us = None;
        central.on_timer(now_us);
        self.sync(c, central, now_us, entropy, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::central::{Step, StepStatus, Target, Uuid};

    const PUBLIC: [u8; 6] = [0x03, 0x00, 0xC3, 0x00, 0x00, 0x02];

    fn cmd(c: &mut Controller, op: u16, params: &[u8]) {
        let mut p = vec![hci::H4_COMMAND];
        p.extend_from_slice(&op.to_le_bytes());
        p.push(params.len() as u8);
        p.extend_from_slice(params);
        let events = c.on_packet(PUBLIC, &p, &mut |b: &mut [u8]| b.fill(0));
        assert!(!events.is_empty(), "{op:#06x} answered");
    }

    fn counter(seed: u8) -> impl FnMut(&mut [u8]) {
        let mut n = seed;
        move |b: &mut [u8]| {
            for x in b.iter_mut() {
                n = n.wrapping_mul(31).wrapping_add(7);
                *x = n;
            }
        }
    }

    /// A controller advertising `kind` every 20 ms with the Passport Keys vendor service.
    fn advertising(kind: u8) -> Controller {
        let mut c = Controller::default();
        cmd(&mut c, 0x0C01, &[0xFF; 8]);
        cmd(
            &mut c,
            0x2001,
            &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00],
        );
        let mut params = vec![0x20, 0x00, 0x20, 0x00, kind, 0x00, 0x00];
        params.extend_from_slice(&[0; 6]);
        params.extend_from_slice(&[0x07, 0x00]);
        cmd(&mut c, 0x2006, &params);
        let mut data = vec![0u8; 32];
        data[0] = 5;
        data[1..6].copy_from_slice(&[0x02, 0x01, 0x06, 0x01, 0xFF]);
        cmd(&mut c, 0x2008, &data);
        cmd(&mut c, 0x200A, &[1]);
        c
    }

    fn run_advertising(
        air: &mut Air,
        c: &mut Controller,
        central: &mut Central,
        rng: &mut dyn FnMut(&mut [u8]),
        at: u64,
    ) -> AirOut {
        air.on_advertising_timer(c, central, PUBLIC, at, rng)
    }

    #[test]
    fn advertising_events_follow_the_interval_plus_a_bounded_delay_on_all_three_channels() {
        let (mut c, mut central, mut air) = (advertising(0), Central::default(), Air::default());
        let mut rng = counter(1);
        let mut out = AirOut::default();
        air.sync(&mut c, &central, 1_000, &mut rng, &mut out);
        let first = air.adv_timer_us.expect("armed");
        assert!((1_000..=1_000 + ADV_DELAY_MAX_US).contains(&first));
        assert_eq!(out.timers, [(first, TIMER_ADVERTISING)]);
        let mut again = AirOut::default();
        air.sync(&mut c, &central, 2_000, &mut rng, &mut again);
        assert!(again.timers.is_empty());
        assert_eq!(
            run_advertising(&mut air, &mut c, &mut central, &mut rng, first + 1),
            AirOut::default()
        );
        let mut at = first;
        for event in 0..20 {
            let out = run_advertising(&mut air, &mut c, &mut central, &mut rng, at);
            let tail: Vec<_> = air.log.iter().rev().take(3).rev().collect();
            assert_eq!(
                tail.iter()
                    .map(|r| (r.channel, r.at_us - at))
                    .collect::<Vec<_>>(),
                [
                    (37, 0),
                    (38, CHANNEL_SPACING_US),
                    (39, 2 * CHANNEL_SPACING_US)
                ],
                "event {event}"
            );
            let pdu = AdvertisingPdu::from_air(tail[0]).expect("an advertising PDU");
            assert_eq!(pdu.pdu_type, pdu::ADV_IND);
            assert_eq!(pdu.adv_a, PUBLIC);
            assert_eq!(pdu.adv_data, [0x02, 0x01, 0x06, 0x01, 0xFF]);
            let next = air.adv_timer_us.expect("rearmed");
            assert!(
                (20_000..=20_000 + ADV_DELAY_MAX_US).contains(&(next - at)),
                "{}",
                next - at
            );
            assert!(out.timers.contains(&(next, TIMER_ADVERTISING)));
            at = next;
        }
        assert_eq!(air.adv_events, 20);
        cmd(&mut c, 0x200A, &[0]);
        let mut out = AirOut::default();
        air.sync(&mut c, &central, at - 1, &mut rng, &mut out);
        assert_eq!(air.adv_timer_us, None);
        assert_eq!(
            run_advertising(&mut air, &mut c, &mut central, &mut rng, at),
            AirOut::default()
        );
        assert_eq!(air.adv_events, 20);
    }

    #[test]
    fn the_same_seed_puts_the_same_pdus_at_the_same_instants() {
        let trace = |seed: u8| {
            let (mut c, mut central, mut air) =
                (advertising(2), Central::default(), Air::default());
            let mut rng = counter(seed);
            let mut out = AirOut::default();
            air.sync(&mut c, &central, 0, &mut rng, &mut out);
            for _ in 0..10 {
                let at = air.adv_timer_us.expect("armed");
                run_advertising(&mut air, &mut c, &mut central, &mut rng, at);
            }
            air.log.iter().map(|r| r.at_us).collect::<Vec<_>>()
        };
        assert_eq!(trace(3), trace(3));
        assert_ne!(trace(3), trace(4));
    }

    #[test]
    fn channel_map_and_filter_policy_shape_the_event() {
        let mut c = advertising(0);
        cmd(&mut c, 0x200A, &[0]);
        let mut params = vec![0x20, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00];
        params.extend_from_slice(&[0; 6]);
        // Channels 38 and 39 only; connections from accept-listed initiators only.
        params.extend_from_slice(&[0x06, 0x02]);
        cmd(&mut c, 0x2006, &params);
        cmd(&mut c, 0x200A, &[1]);
        let (mut central, mut air) = (Central::default(), Air::default());
        central.push_steps(
            vec![Step::Connect {
                target: Target::Any,
                interval: 24,
                latency: 0,
                timeout: 400,
                within_ms: 1_000,
            }],
            0,
        );
        let mut rng = counter(9);
        let mut out = AirOut::default();
        air.sync(&mut c, &central, 0, &mut rng, &mut out);
        let at = air.adv_timer_us.expect("armed");
        run_advertising(&mut air, &mut c, &mut central, &mut rng, at);
        assert_eq!(
            air.log.iter().map(|r| r.channel).collect::<Vec<_>>(),
            [38, 39]
        );
        assert!(c.connections.is_empty(), "the central is not accept-listed");
    }

    /// A connection made by the central on `c`'s advertising, with the air at its first anchor.
    fn connect(
        c: &mut Controller,
        air: &mut Air,
        central: &mut Central,
        rng: &mut dyn FnMut(&mut [u8]),
    ) -> (AirOut, u64) {
        central.push_steps(
            vec![Step::Connect {
                target: Target::Any,
                interval: 24,
                latency: 0,
                timeout: 400,
                within_ms: 1_000,
            }],
            0,
        );
        let mut out = AirOut::default();
        air.sync(c, central, 0, rng, &mut out);
        let at = air.adv_timer_us.expect("armed");
        let out = air.on_advertising_timer(c, central, PUBLIC, at, rng);
        (out, at)
    }

    #[test]
    fn a_connect_ind_ends_advertising_and_reports_the_connection_to_the_host() {
        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(5);
        let (out, at) = connect(&mut c, &mut air, &mut central, &mut rng);
        assert!(!c.advertising.enabled);
        assert_eq!(air.adv_timer_us, None);
        assert_eq!(c.connections.len(), 1);
        let handle = c.connections[0].handle;
        assert_eq!(handle, 1);
        // ADV_IND on 37, CONNECT_IND from the central T_IFS after it on the same channel.
        let (adv, ind) = (&air.log[0], &air.log[1]);
        assert_eq!((adv.channel, ind.channel, ind.from_central), (37, 37, true));
        assert_eq!(ind.pdu[0] & 0x0F, pdu::CONNECT_IND);
        assert_eq!(ind.pdu[1], 34);
        assert_eq!(ind.at_us, at + pdu_air_us(adv.pdu.len() - 2) + T_IFS_US);
        // LL data: interval 24, latency 0, timeout 400 at octets 22 to 27 of the payload.
        assert_eq!(&ind.pdu[2 + 22..2 + 28], &[24, 0, 0, 0, 0x90, 0x01]);
        // The host hears LE Enhanced Connection Complete [v2] (the mask enables bit 40) at the
        // CONNECT_IND instant: peripheral role, the central's public address.
        let (due, event) = &out.events[0];
        assert_eq!(*due, ind.at_us);
        assert_eq!(&event[..4], &[hci::H4_EVENT, hci::event::LE_META, 34, 0x29]);
        assert_eq!(&event[4..8], &[0x00, 0x01, 0x00, 0x01]);
        assert_eq!(event[8], 0x00);
        assert_eq!(&event[9..15], &central::CENTRAL_ADDRESS);
        assert_eq!(
            &event[27..34],
            &[24, 0, 0, 0, 0x90, 0x01, central::CENTRAL_SCA]
        );
        let anchor = air.conn_timer_us.expect("the first connection event");
        assert_eq!(
            anchor,
            ind.at_us + pdu_air_us(34) + TRANSMIT_WINDOW_DELAY_US
        );
        assert!(out.timers.contains(&(anchor, TIMER_CONNECTION)));
        assert_eq!(central.connected_handle(), Some(1));
        assert_eq!(central.results[0].status, StepStatus::Ok);

        // With only bit 9, the Enhanced [v1] event; with only bit 0, the legacy one.
        for (le_mask, sub, len) in [(0x0200u64, 0x0A, 31), (0x0001, 0x01, 19)] {
            let mut c = advertising(0);
            cmd(&mut c, 0x2001, &le_mask.to_le_bytes());
            let (mut central, mut air) = (Central::default(), Air::default());
            let (out, _) = connect(&mut c, &mut air, &mut central, &mut rng);
            assert_eq!(out.events[0].1[2..4], [len, sub]);
        }
    }

    #[test]
    fn host_packets_are_completed_at_the_connection_event_and_central_frames_reach_the_host() {
        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(7);
        connect(&mut c, &mut air, &mut central, &mut rng);
        let anchor = air.conn_timer_us.expect("armed");
        // The host sends an ATT Exchange MTU Request before the first event: nothing completes yet.
        let request = hci::acl(
            1,
            hci::PB_FIRST_NON_FLUSHABLE,
            &[3, 0, 4, 0, 0x02, 0x00, 0x02],
        );
        assert!(
            c.on_packet(PUBLIC, &request, &mut |b: &mut [u8]| b.fill(0))
                .is_empty()
        );
        assert_eq!(
            air.on_connection_timer(&mut c, &mut central, anchor - 1, &mut rng),
            AirOut::default()
        );
        let out = air.on_connection_timer(&mut c, &mut central, anchor, &mut rng);
        assert_eq!(
            out.events,
            [(anchor, hci::number_of_completed_packets(&[(1, 1)]))],
            "completed at the event, where the central received it"
        );
        let next = anchor + 24 * 1_250;
        assert_eq!(air.conn_timer_us, Some(next));
        // The central answered with its MTU, which goes out at the next event as one fragment.
        let out = air.on_connection_timer(&mut c, &mut central, next, &mut rng);
        assert_eq!(
            out.events,
            [(
                next,
                hci::acl(1, hci::PB_FIRST_FLUSHABLE, &[3, 0, 4, 0, 0x03, 247, 0])
            )]
        );
        // A long central frame is split into 27-octet fragments, at most PDUS_PER_EVENT per event.
        central
            .link
            .as_mut()
            .expect("link")
            .tx
            .push(vec![0xAB; 27 * 5 + 1]);
        let third = next + 30_000;
        let out = air.on_connection_timer(&mut c, &mut central, third, &mut rng);
        let flags: Vec<u8> = out.events.iter().map(|(_, p)| p[2] >> 4).collect();
        assert_eq!(flags, [hci::PB_FIRST_FLUSHABLE, 1, 1, 1]);
        let out = air.on_connection_timer(&mut c, &mut central, third + 30_000, &mut rng);
        assert_eq!(out.events.len(), 2);
        assert_eq!(out.events[1].1[3], 1, "the last fragment holds one octet");
        let hop = air.link.as_ref().expect("link").hop;
        assert!((5..=16).contains(&hop));
        assert_eq!(
            air.link.as_ref().map(|l| l.last_channel),
            Some((4 * hop) % 37)
        );
    }

    #[test]
    fn a_central_disconnect_and_a_host_disconnect_end_the_link() {
        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(11);
        connect(&mut c, &mut air, &mut central, &mut rng);
        central.push_steps(vec![Step::Disconnect], 10);
        let anchor = air.conn_timer_us.expect("armed");
        let out = air.on_connection_timer(&mut c, &mut central, anchor, &mut rng);
        assert_eq!(
            out.events,
            [(
                anchor,
                hci::event(hci::event::DISCONNECTION_COMPLETE, &[0, 1, 0, 0x13])
            )]
        );
        assert_eq!((air.link.as_ref(), air.conn_timer_us), (None, None));
        assert!(c.connections.is_empty());
        assert_eq!(
            central.results.last().map(|r| r.status),
            Some(StepStatus::Ok)
        );
        assert_eq!(
            air.log.last().map(|r| r.pdu.clone()),
            Some(vec![0b11, 2, 0x02, 0x13])
        );

        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        connect(&mut c, &mut air, &mut central, &mut rng);
        central.push_steps(
            vec![Step::Read {
                characteristic: Uuid::from_u16(0x2A00),
            }],
            10,
        );
        cmd(&mut c, 0x0406, &[1, 0, 0x13]);
        assert_eq!(
            c.terminating(1),
            Some(0x13),
            "the air still has to carry it"
        );
        // The first event puts LL_TERMINATE_IND on the air from the peripheral; the host hears
        // nothing yet (§5.1.3).
        let anchor = air.conn_timer_us.expect("armed");
        let out = air.on_connection_timer(&mut c, &mut central, anchor, &mut rng);
        assert!(out.events.is_empty());
        let last = air.log.last().expect("a PDU");
        assert_eq!(
            (last.from_central, last.pdu.clone()),
            (false, vec![0b11, 2, 0x02, 0x13])
        );
        assert_eq!(c.connections.len(), 1, "still open until acknowledged");
        assert_eq!(central.link, None, "the central heard the termination");
        // The next event carries the acknowledgement and the host's Disconnection Complete.
        let next = air.conn_timer_us.expect("armed");
        let out = air.on_connection_timer(&mut c, &mut central, next, &mut rng);
        assert_eq!(
            out.events,
            [(
                next,
                hci::event(
                    hci::event::DISCONNECTION_COMPLETE,
                    &[0, 1, 0, hci::status::LOCAL_HOST_TERMINATED]
                )
            )]
        );
        assert!(air.log.last().expect("a PDU").from_central, "the ack");
        assert_eq!(air.link, None);
        assert!(c.connections.is_empty());
    }

    #[test]
    fn an_l2cap_parameter_update_the_central_accepts_is_applied_at_the_next_event() {
        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(13);
        connect(&mut c, &mut air, &mut central, &mut rng);
        let request = hci::acl(
            1,
            0,
            &[12, 0, 5, 0, 0x12, 1, 8, 0, 12, 0, 12, 0, 0, 0, 0x90, 0x01],
        );
        c.on_packet(PUBLIC, &request, &mut |b: &mut [u8]| b.fill(0));
        let anchor = air.conn_timer_us.expect("armed");
        air.on_connection_timer(&mut c, &mut central, anchor, &mut rng);
        let next = air.conn_timer_us.expect("armed");
        let out = air.on_connection_timer(&mut c, &mut central, next, &mut rng);
        assert_eq!(
            out.events[0].1[..4],
            [hci::H4_EVENT, hci::event::LE_META, 10, 0x03]
        );
        assert_eq!(c.connections[0].interval, 12);
        assert_eq!(air.conn_timer_us, Some(next + 12 * 1_250));
    }

    #[test]
    fn high_duty_cycle_directed_advertising_times_out() {
        let mut c = Controller::default();
        cmd(&mut c, 0x0C01, &[0xFF; 8]);
        cmd(&mut c, 0x2001, &[0x1F, 0, 0, 0, 0, 0, 0, 0]);
        let mut params = vec![0, 0, 0, 0, 0x01, 0x00, 0x00];
        params.extend_from_slice(&[9, 9, 9, 9, 9, 9]);
        params.extend_from_slice(&[0x07, 0x00]);
        cmd(&mut c, 0x2006, &params);
        cmd(&mut c, 0x200A, &[1]);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(2);
        let mut out = AirOut::default();
        air.sync(&mut c, &central, 0, &mut rng, &mut out);
        assert_eq!(air.adv_timer_us, Some(0), "no advDelay");
        let (mut last, mut last_event_us) = (AirOut::default(), 0);
        while let Some(at) = air.adv_timer_us {
            let before = air.adv_events;
            last = run_advertising(&mut air, &mut c, &mut central, &mut rng, at);
            if air.adv_events != before {
                last_event_us = at;
            }
        }
        // The last event starts strictly before 1.28 s.
        assert_eq!(
            air.adv_events,
            HIGH_DUTY_TIMEOUT_US.div_ceil(HIGH_DUTY_INTERVAL_US)
        );
        assert!(
            last_event_us < HIGH_DUTY_TIMEOUT_US,
            "the last event starts inside the 1.28 s"
        );
        assert_eq!(
            last_event_us,
            (air.adv_events - 1) * HIGH_DUTY_INTERVAL_US,
            "no advDelay"
        );
        assert!(!c.advertising.enabled);
        assert_eq!(
            last.events[0].1[3..5],
            [0x01, hci::status::ADVERTISING_TIMEOUT]
        );
        let pdu = AdvertisingPdu::from_air(&air.log[0]).expect("advertising");
        assert_eq!(pdu.pdu_type, pdu::ADV_DIRECT_IND);
        assert_eq!(&air.log[0].pdu[8..14], &[9; 6], "TargetA");
    }

    #[test]
    fn the_air_state_round_trips_through_its_snapshot_codec() {
        use pemu_core::snap::{SnapReader, SnapValue};
        let mut c = advertising(0);
        let (mut central, mut air) = (Central::default(), Air::default());
        let mut rng = counter(3);
        connect(&mut c, &mut air, &mut central, &mut rng);
        air.link
            .as_mut()
            .expect("link")
            .to_peripheral
            .push(Fragment {
                start: true,
                data: vec![1, 2],
                last: true,
            });
        air.link.as_mut().expect("link").to_central.push(Fragment {
            start: false,
            data: vec![3],
            last: false,
        });
        air.link.as_mut().expect("link").terminate_sent = true;
        air.central_timer_us = Some(7);
        let mut bytes = Vec::new();
        air.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "test");
        assert_eq!(Air::snap_read(&mut r), Ok(air));
        assert!(r.is_empty());
    }
}
