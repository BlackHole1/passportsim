//! The scripted central of the virtual air: it scans, connects to the emulated peripheral and runs
//! a GATT client, one journaled [`Step`] at a time, in virtual time. Written from the Bluetooth
//! Core Specification 5.4 Vol 3 Parts A (L2CAP), F (ATT), G (GATT) and H §3.5.5 (SMP).
//!
//! - A step that needs a connection and has none ends [`StepStatus::NotConnected`]; one whose
//!   response misses the 30 s ATT transaction timeout (Vol 3 Part F §3.3.3) or its own
//!   `within_ms` ends [`StepStatus::Timeout`].
//! - Scanning is active: one `SCAN_REQ` per scannable advertiser per scan step.
//! - One ATT request at a time (Vol 3 Part F §3.3.2). Discovery follows Vol 3 Part G §4.4.1,
//!   §4.6.1 and §4.7.1, each repeated from the handle after the last one returned.
//! - Peripheral requests are answered minimally: `Exchange MTU` with the central's MTU, discovery
//!   with `Attribute Not Found`, handle reads and writes with `Invalid Handle`, anything else with
//!   `Request Not Supported`. A connection parameter update is applied; other signaling requests
//!   get `Command Reject`. Every SMP PDU gets `Pairing Not Supported`: Passport Keys does not pair.
//!
//! [`Central`] is part of `vhci::BleState`, so a snapshot mid-exchange restores it mid-step.

use pemu_core::snap::{SnapError, SnapReader, SnapValue};

/// ATT opcodes (Core Vol 3 Part F §3.4.8).
pub mod att {
    pub const ERROR_RSP: u8 = 0x01;
    pub const EXCHANGE_MTU_REQ: u8 = 0x02;
    pub const EXCHANGE_MTU_RSP: u8 = 0x03;
    pub const FIND_INFORMATION_REQ: u8 = 0x04;
    pub const FIND_INFORMATION_RSP: u8 = 0x05;
    pub const FIND_BY_TYPE_VALUE_REQ: u8 = 0x06;
    pub const READ_BY_TYPE_REQ: u8 = 0x08;
    pub const READ_BY_TYPE_RSP: u8 = 0x09;
    pub const READ_REQ: u8 = 0x0A;
    pub const READ_RSP: u8 = 0x0B;
    pub const READ_BLOB_REQ: u8 = 0x0C;
    pub const READ_MULTIPLE_REQ: u8 = 0x0E;
    pub const READ_BY_GROUP_TYPE_REQ: u8 = 0x10;
    pub const READ_BY_GROUP_TYPE_RSP: u8 = 0x11;
    pub const WRITE_REQ: u8 = 0x12;
    pub const WRITE_RSP: u8 = 0x13;
    pub const HANDLE_VALUE_NTF: u8 = 0x1B;
    pub const HANDLE_VALUE_IND: u8 = 0x1D;
    pub const HANDLE_VALUE_CFM: u8 = 0x1E;
    pub const WRITE_CMD: u8 = 0x52;

    pub const ERR_INVALID_HANDLE: u8 = 0x01;
    pub const ERR_REQUEST_NOT_SUPPORTED: u8 = 0x06;
    pub const ERR_ATTRIBUTE_NOT_FOUND: u8 = 0x0A;
}

/// L2CAP channel ids of an LE-U logical link (Core Vol 3 Part A §2.1).
pub mod cid {
    pub const ATT: u16 = 0x0004;
    pub const SIGNALING: u16 = 0x0005;
    pub const SMP: u16 = 0x0006;
}

/// Core Vol 3 Part F §3.2.8.
pub const DEFAULT_ATT_MTU: u16 = 23;
/// Core Vol 3 Part F §3.3.3.
pub const ATT_TIMEOUT_US: u64 = 30_000_000;
/// Advertisers kept by a scan, newest replacing the oldest.
pub const MAX_REPORTS: usize = 16;
pub const MAX_NOTIFICATIONS: usize = 64;
pub const MAX_RESULTS: usize = 64;
pub const MAX_STEPS: usize = 64;
/// The largest ATT attribute value (Vol 3 Part F §3.2.9).
pub const MAX_VALUE: usize = 512;
/// `02:00:00:00:00:01` public, least significant octet first: a placeholder, class C. Addresses
/// the agent supplies are never redacted.
pub const CENTRAL_ADDRESS: [u8; 6] = [0x01, 0x00, 0x00, 0x00, 0x00, 0x02];
/// 50 ppm (code 5 of Vol 6 Part B §2.3.3.1 `SCA`), class C.
pub const CENTRAL_SCA: u8 = 5;

/// A 128-bit UUID, least significant octet first as ATT carries it; a 16-bit UUID is stored in its
/// Bluetooth Base UUID form (Core Vol 3 Part B §2.5.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Uuid(pub [u8; 16]);

const BASE_UUID: [u8; 16] = [
    0xFB, 0x34, 0x9B, 0x5F, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

impl Uuid {
    pub const fn from_u16(v: u16) -> Uuid {
        let mut b = BASE_UUID;
        b[12] = v as u8;
        b[13] = (v >> 8) as u8;
        Uuid(b)
    }

    pub fn parse(text: &str) -> Option<Uuid> {
        let hex: String = text.chars().filter(|c| *c != '-').collect();
        if hex.len() == 4 {
            return u16::from_str_radix(&hex, 16).ok().map(Uuid::from_u16);
        }
        if hex.len() != 32 || text.len() != 36 {
            return None;
        }
        let mut b = [0u8; 16];
        for (i, byte) in b.iter_mut().enumerate() {
            // The string is most significant first.
            *byte = u8::from_str_radix(hex.get(2 * (15 - i)..2 * (15 - i) + 2)?, 16).ok()?;
        }
        Some(Uuid(b))
    }

    pub fn from_att(bytes: &[u8]) -> Option<Uuid> {
        match bytes.len() {
            2 => Some(Uuid::from_u16(u16::from_le_bytes([bytes[0], bytes[1]]))),
            16 => bytes.try_into().ok().map(Uuid),
            _ => None,
        }
    }

    /// 2 octets for a Base UUID, else 16.
    pub fn to_att(self) -> Vec<u8> {
        if self.0[..12] == BASE_UUID[..12] && self.0[14..] == BASE_UUID[14..] {
            self.0[12..14].to_vec()
        } else {
            self.0.to_vec()
        }
    }
}

/// GATT attribute types (Core Vol 3 Part G §3.1 to §3.3).
pub mod gatt {
    use super::Uuid;
    pub const PRIMARY_SERVICE: Uuid = Uuid::from_u16(0x2800);
    pub const CHARACTERISTIC: Uuid = Uuid::from_u16(0x2803);
    pub const CCCD: Uuid = Uuid::from_u16(0x2902);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Any,
    Address {
        address: [u8; 6],
        random: bool,
    },
    /// The first connectable advertiser whose advertising or scan response data lists this service
    /// (AD types 0x02 to 0x07, Core Supplement Part A §1.1).
    Service(Uuid),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Scan {
        ms: u32,
    },
    /// Connection parameters in 1.25 ms, events and 10 ms units.
    Connect {
        target: Target,
        interval: u16,
        latency: u16,
        timeout: u16,
        /// How long to wait for a connectable advertisement.
        within_ms: u32,
    },
    ExchangeMtu {
        mtu: u16,
    },
    DiscoverServices,
    /// Discovers services first when none are known.
    DiscoverCharacteristics {
        service: Uuid,
    },
    Subscribe {
        characteristic: Uuid,
        indicate: bool,
    },
    /// `ATT_WRITE_REQ` when `with_response`, else `ATT_WRITE_CMD`.
    Write {
        characteristic: Uuid,
        value: Vec<u8>,
        with_response: bool,
    },
    Read {
        characteristic: Uuid,
    },
    /// Waits until the values notified on `characteristic` since the last matched wait contain
    /// `contains`.
    WaitNotification {
        characteristic: Uuid,
        contains: Vec<u8>,
        within_ms: u32,
    },
    Wait {
        ms: u32,
    },
    /// `LL_TERMINATE_IND` with Remote User Terminated Connection.
    Disconnect,
    /// Applied by the module to `btsnoop::Capture` when the script is journaled, so it never
    /// reaches the central and produces no result.
    Capture {
        enabled: bool,
    },
    /// Keep key material in the capture. Like [`Step::Capture`] it produces no result. It taints
    /// the instance, so it is journaled: a replay records the same way and carries the same taint.
    CaptureSecrets {
        secrets: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Timeout,
    NotConnected,
    NotFound,
    AttError(u8),
    TooLong,
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepResult {
    /// Index of the step since the central was created.
    pub index: u32,
    pub at_us: u64,
    pub status: StepStatus,
    /// A read's value; otherwise empty.
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AdvReport {
    pub at_us: u64,
    pub channel: u8,
    pub pdu_type: u8,
    pub address: [u8; 6],
    pub random: bool,
    pub data: Vec<u8>,
    /// Set once a `SCAN_RSP` arrives.
    pub scan_response: Option<Vec<u8>>,
}

/// Core Vol 3 Part G §4.4.1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Service {
    pub start: u16,
    pub end: u16,
    pub uuid: Uuid,
}

/// Core Vol 3 Part G §3.3.1, §4.6.1, §4.7.1.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Characteristic {
    pub declaration: u16,
    pub properties: u8,
    pub value_handle: u16,
    pub uuid: Uuid,
    pub cccd: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    pub at_us: u64,
    pub handle: u16,
    pub value: Vec<u8>,
    pub indication: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvAction {
    None,
    ScanRequest,
    Connect {
        /// 1.25 ms units.
        interval: u16,
        latency: u16,
        /// 10 ms units.
        timeout: u16,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlRequest {
    /// Core Vol 6 Part B §5.1.1.
    Update {
        /// 1.25 ms units.
        interval: u16,
        latency: u16,
        /// 10 ms units.
        timeout: u16,
    },
    /// Core Vol 6 Part B §5.1.3.
    Terminate { reason: u8 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Running {
    pub index: u32,
    pub step: Step,
    /// When it times out or, for a scan or wait, ends.
    pub deadline_us: Option<u64>,
    /// Discovery phase: 0 services, 1 characteristics, 2 descriptors.
    pub phase: u8,
    pub cursor: u16,
    /// Address type and address of each advertiser already sent a `SCAN_REQ` in this step.
    pub scan_requested: Vec<[u8; 7]>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CentralLink {
    pub handle: u16,
    pub mtu: u16,
    pub rx: Vec<u8>,
    pub tx: Vec<Vec<u8>>,
    /// The request opcode whose response is awaited.
    pub pending: Option<u8>,
    pub ll: Vec<LlRequest>,
    pub terminating: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Central {
    pub next_index: u32,
    pub queue: Vec<Step>,
    pub running: Option<Running>,
    pub results: Vec<StepResult>,
    pub reports: Vec<AdvReport>,
    pub link: Option<CentralLink>,
    pub services: Vec<Service>,
    pub characteristics: Vec<Characteristic>,
    pub notifications: Vec<Notification>,
    /// Received since the central was created.
    pub notified: u64,
    /// The count of [`Central::notified`] a [`Step::WaitNotification`] searches from.
    pub wait_from: u64,
    pub refused_steps: u64,
}

fn le16(b: &[u8], at: usize) -> u16 {
    match b.get(at..at + 2) {
        Some(w) => u16::from_le_bytes([w[0], w[1]]),
        None => 0,
    }
}

/// Core Supplement Part A §1.1: AD types 0x02 and 0x03 hold 16-bit, 0x04 and 0x05 32-bit, 0x06
/// and 0x07 128-bit UUIDs.
pub fn ad_lists_service(data: &[u8], uuid: Uuid) -> bool {
    let mut at = 0;
    while at < data.len() {
        let len = usize::from(data[at]);
        if len == 0 || at + 1 + len > data.len() {
            return false;
        }
        let (kind, body) = (data[at + 1], &data[at + 2..at + 1 + len]);
        let found = match kind {
            0x02 | 0x03 => body
                .chunks_exact(2)
                .any(|u| Uuid::from_att(u) == Some(uuid)),
            0x04 | 0x05 => body.chunks_exact(4).any(|u| {
                let mut b = BASE_UUID;
                b[12..16].copy_from_slice(u);
                Uuid(b) == uuid
            }),
            0x06 | 0x07 => body.chunks_exact(16).any(|u| u == uuid.0),
            _ => false,
        };
        if found {
            return true;
        }
        at += 1 + len;
    }
    false
}

/// The complete (0x09) or shortened (0x08) Local Name (Core Supplement Part A §1.2), or `None`
/// when absent or not UTF-8.
pub fn ad_local_name(data: &[u8]) -> Option<String> {
    let mut at = 0;
    while at < data.len() {
        let len = usize::from(data[at]);
        if len == 0 || at + 1 + len > data.len() {
            return None;
        }
        let (kind, body) = (data[at + 1], &data[at + 2..at + 1 + len]);
        if kind == 0x09 || kind == 0x08 {
            return core::str::from_utf8(body).ok().map(str::to_owned);
        }
        at += 1 + len;
    }
    None
}

fn l2cap(channel: u16, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u16).to_le_bytes().to_vec();
    out.extend_from_slice(&channel.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

fn att_error(request: u8, handle: u16, code: u8) -> Vec<u8> {
    let [lo, hi] = handle.to_le_bytes();
    vec![att::ERROR_RSP, request, lo, hi, code]
}

impl Central {
    /// Steps beyond [`MAX_STEPS`] queued are refused and counted.
    pub fn push_steps(&mut self, steps: Vec<Step>, now_us: u64) {
        for step in steps {
            if self.queue.len() >= MAX_STEPS {
                self.refused_steps += 1;
            } else {
                self.queue.push(step);
            }
        }
        self.advance(now_us);
    }

    pub fn timer_us(&self) -> Option<u64> {
        self.running.as_ref().and_then(|r| r.deadline_us)
    }

    pub fn connected_handle(&self) -> Option<u16> {
        self.link.as_ref().map(|l| l.handle)
    }

    pub fn result(&self, index: u32) -> Option<&StepResult> {
        self.results.iter().find(|r| r.index == index)
    }

    fn finish(&mut self, now_us: u64, status: StepStatus, value: Vec<u8>) {
        let Some(running) = self.running.take() else {
            return;
        };
        if let Some(link) = self.link.as_mut() {
            link.pending = None;
        }
        self.results.push(StepResult {
            index: running.index,
            at_us: now_us,
            status,
            value,
        });
        if self.results.len() > MAX_RESULTS {
            self.results.remove(0);
        }
    }

    fn advance(&mut self, now_us: u64) {
        while self.running.is_none() && !self.queue.is_empty() {
            let step = self.queue.remove(0);
            let index = self.next_index;
            self.next_index += 1;
            self.running = Some(Running {
                index,
                step: step.clone(),
                deadline_us: None,
                phase: 0,
                cursor: 1,
                scan_requested: Vec::new(),
            });
            self.start(step, now_us);
        }
    }

    fn set_deadline(&mut self, at_us: u64) {
        if let Some(r) = self.running.as_mut() {
            r.deadline_us = Some(at_us);
        }
    }

    fn request(&mut self, pdu: Vec<u8>, now_us: u64) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        link.pending = Some(pdu[0]);
        link.tx.push(l2cap(cid::ATT, &pdu));
        let own = match self.running.as_ref().map(|r| &r.step) {
            Some(Step::WaitNotification { .. }) => None,
            _ => Some(now_us + ATT_TIMEOUT_US),
        };
        if let Some(at) = own {
            self.set_deadline(at);
        }
    }

    fn characteristic(&self, uuid: Uuid) -> Option<Characteristic> {
        self.characteristics
            .iter()
            .find(|c| c.uuid == uuid)
            .cloned()
    }

    fn start(&mut self, step: Step, now_us: u64) {
        let connected = self.link.is_some();
        match step {
            Step::Scan { ms } | Step::Wait { ms } => {
                self.set_deadline(now_us + u64::from(ms) * 1_000);
            }
            // The module removes these before the central sees them.
            Step::Capture { .. } | Step::CaptureSecrets { .. } => {
                self.finish(now_us, StepStatus::Ok, Vec::new())
            }
            Step::Connect { within_ms, .. } => {
                if connected {
                    self.finish(now_us, StepStatus::Ok, Vec::new());
                } else {
                    self.set_deadline(now_us + u64::from(within_ms) * 1_000);
                }
            }
            _ if !connected => self.finish(now_us, StepStatus::NotConnected, Vec::new()),
            Step::ExchangeMtu { mtu } => {
                let [lo, hi] = mtu.max(DEFAULT_ATT_MTU).to_le_bytes();
                self.request(vec![att::EXCHANGE_MTU_REQ, lo, hi], now_us);
            }
            Step::DiscoverServices => {
                self.services.clear();
                self.characteristics.clear();
                self.discover_services(1, now_us);
            }
            Step::DiscoverCharacteristics { service } => {
                match self.services.iter().find(|s| s.uuid == service).cloned() {
                    Some(svc) => self.discover_characteristics(&svc, svc.start, now_us),
                    None => self.discover_services(1, now_us),
                }
            }
            Step::Subscribe {
                characteristic,
                indicate,
            } => match self.characteristic(characteristic).and_then(|c| c.cccd) {
                Some(cccd) => {
                    let [lo, hi] = cccd.to_le_bytes();
                    let value = if indicate { 0x02 } else { 0x01 };
                    self.request(vec![att::WRITE_REQ, lo, hi, value, 0x00], now_us);
                }
                None => self.finish(now_us, StepStatus::NotFound, Vec::new()),
            },
            Step::Write {
                characteristic,
                value,
                with_response,
            } => {
                let mtu = self.link.as_ref().map_or(DEFAULT_ATT_MTU, |l| l.mtu);
                let Some(c) = self.characteristic(characteristic) else {
                    return self.finish(now_us, StepStatus::NotFound, Vec::new());
                };
                if value.len() + 3 > usize::from(mtu) || value.len() > MAX_VALUE {
                    return self.finish(now_us, StepStatus::TooLong, Vec::new());
                }
                let [lo, hi] = c.value_handle.to_le_bytes();
                let op = if with_response {
                    att::WRITE_REQ
                } else {
                    att::WRITE_CMD
                };
                let mut pdu = vec![op, lo, hi];
                pdu.extend_from_slice(&value);
                if with_response {
                    self.request(pdu, now_us);
                } else {
                    if let Some(link) = self.link.as_mut() {
                        link.tx.push(l2cap(cid::ATT, &pdu));
                    }
                    self.finish(now_us, StepStatus::Ok, Vec::new());
                }
            }
            Step::Read { characteristic } => match self.characteristic(characteristic) {
                Some(c) => {
                    let [lo, hi] = c.value_handle.to_le_bytes();
                    self.request(vec![att::READ_REQ, lo, hi], now_us);
                }
                None => self.finish(now_us, StepStatus::NotFound, Vec::new()),
            },
            Step::WaitNotification { within_ms, .. } => {
                self.set_deadline(now_us + u64::from(within_ms) * 1_000);
                self.check_wait(now_us);
            }
            Step::Disconnect => {
                if let Some(link) = self.link.as_mut()
                    && !link.terminating
                {
                    link.terminating = true;
                    link.ll.push(LlRequest::Terminate {
                        reason: crate::ble::hci::status::REMOTE_USER_TERMINATED,
                    });
                }
            }
        }
    }
}

impl Central {
    fn discover_services(&mut self, from: u16, now_us: u64) {
        if let Some(r) = self.running.as_mut() {
            r.phase = 0;
            r.cursor = from;
        }
        let [lo, hi] = from.to_le_bytes();
        let mut pdu = vec![att::READ_BY_GROUP_TYPE_REQ, lo, hi, 0xFF, 0xFF];
        pdu.extend_from_slice(&gatt::PRIMARY_SERVICE.to_att());
        self.request(pdu, now_us);
    }

    fn discover_characteristics(&mut self, svc: &Service, from: u16, now_us: u64) {
        if from > svc.end || from == 0 {
            return self.discover_descriptors(svc, svc.start, now_us);
        }
        if let Some(r) = self.running.as_mut() {
            r.phase = 1;
            r.cursor = from;
        }
        let mut pdu = vec![att::READ_BY_TYPE_REQ];
        pdu.extend_from_slice(&from.to_le_bytes());
        pdu.extend_from_slice(&svc.end.to_le_bytes());
        pdu.extend_from_slice(&gatt::CHARACTERISTIC.to_att());
        self.request(pdu, now_us);
    }

    /// Core Vol 3 Part G §4.7.1: from the handle after the value to the handle before the next
    /// declaration or the end of the service.
    fn descriptor_range(&self, svc: &Service, from: u16) -> Option<(u16, u16)> {
        let chars: Vec<&Characteristic> = self
            .characteristics
            .iter()
            .filter(|c| c.declaration >= svc.start && c.declaration <= svc.end)
            .collect();
        for (i, c) in chars.iter().enumerate() {
            let end = chars
                .get(i + 1)
                .map_or(svc.end, |n| n.declaration.saturating_sub(1));
            let start = c.value_handle.saturating_add(1).max(from);
            if start <= end && c.value_handle < u16::MAX {
                return Some((start, end));
            }
        }
        None
    }

    fn discover_descriptors(&mut self, svc: &Service, from: u16, now_us: u64) {
        let Some((start, end)) = self.descriptor_range(svc, from) else {
            return self.finish(now_us, StepStatus::Ok, Vec::new());
        };
        if let Some(r) = self.running.as_mut() {
            r.phase = 2;
            r.cursor = start;
        }
        let mut pdu = vec![att::FIND_INFORMATION_REQ];
        pdu.extend_from_slice(&start.to_le_bytes());
        pdu.extend_from_slice(&end.to_le_bytes());
        self.request(pdu, now_us);
    }

    fn step_service(&self) -> Option<Service> {
        match self.running.as_ref().map(|r| &r.step) {
            Some(Step::DiscoverCharacteristics { service }) => {
                self.services.iter().find(|s| s.uuid == *service).cloned()
            }
            _ => None,
        }
    }

    /// Records the PDU while a scan or connect step runs and answers it with a `SCAN_REQ` or a
    /// `CONNECT_IND`. `to_central` says whether an `ADV_DIRECT_IND` targets this central.
    #[allow(clippy::too_many_arguments)]
    pub fn on_advertising(
        &mut self,
        at_us: u64,
        channel: u8,
        pdu_type: u8,
        address: [u8; 6],
        random: bool,
        data: &[u8],
        to_central: bool,
    ) -> AdvAction {
        use super::air::pdu;
        let Some(running) = self.running.as_ref() else {
            return AdvAction::None;
        };
        let scanning = matches!(running.step, Step::Scan { .. });
        if !scanning && !matches!(running.step, Step::Connect { .. }) {
            return AdvAction::None;
        }
        let key = {
            let mut k = [0u8; 7];
            k[0] = u8::from(random);
            k[1..].copy_from_slice(&address);
            k
        };
        let slot = self
            .reports
            .iter()
            .position(|r| r.address == address && r.random == random);
        let report = match slot {
            Some(i) => &mut self.reports[i],
            None => {
                if self.reports.len() >= MAX_REPORTS {
                    self.reports.remove(0);
                }
                self.reports.push(AdvReport::default());
                self.reports.last_mut().expect("just pushed")
            }
        };
        report.at_us = at_us;
        report.channel = channel;
        report.pdu_type = pdu_type;
        report.address = address;
        report.random = random;
        report.data = data.to_vec();
        let scan_response = report.scan_response.clone();
        match running.step.clone() {
            Step::Scan { .. } => {
                let scannable = matches!(pdu_type, pdu::ADV_IND | pdu::ADV_SCAN_IND);
                let running = self.running.as_mut().expect("checked above");
                if scannable && !running.scan_requested.contains(&key) {
                    running.scan_requested.push(key);
                    return AdvAction::ScanRequest;
                }
                AdvAction::None
            }
            Step::Connect {
                target,
                interval,
                latency,
                timeout,
                ..
            } => {
                let connectable =
                    pdu_type == pdu::ADV_IND || (pdu_type == pdu::ADV_DIRECT_IND && to_central);
                let matches = match target {
                    Target::Any => true,
                    Target::Address {
                        address: a,
                        random: r,
                    } => a == address && r == random,
                    Target::Service(uuid) => {
                        ad_lists_service(data, uuid)
                            || scan_response.is_some_and(|s| ad_lists_service(&s, uuid))
                    }
                };
                if connectable && matches {
                    AdvAction::Connect {
                        interval,
                        latency,
                        timeout,
                    }
                } else {
                    AdvAction::None
                }
            }
            _ => AdvAction::None,
        }
    }

    /// Core Vol 6 Part B §2.3.2.2.
    pub fn on_scan_response(&mut self, at_us: u64, address: [u8; 6], random: bool, data: &[u8]) {
        if let Some(r) = self
            .reports
            .iter_mut()
            .find(|r| r.address == address && r.random == random)
        {
            r.at_us = at_us;
            r.scan_response = Some(data.to_vec());
        }
    }

    pub fn on_connected(&mut self, handle: u16, at_us: u64) {
        self.link = Some(CentralLink {
            handle,
            mtu: DEFAULT_ATT_MTU,
            ..CentralLink::default()
        });
        if matches!(
            self.running.as_ref().map(|r| &r.step),
            Some(Step::Connect { .. })
        ) {
            self.finish(at_us, StepStatus::Ok, Vec::new());
        }
        self.advance(at_us);
    }

    /// A disconnect step is done; any other step that needs the connection ends
    /// [`StepStatus::Disconnected`].
    pub fn on_disconnected(&mut self, at_us: u64) {
        self.link = None;
        match self.running.as_ref().map(|r| &r.step) {
            Some(Step::Disconnect) => self.finish(at_us, StepStatus::Ok, Vec::new()),
            Some(Step::Scan { .. } | Step::Wait { .. } | Step::Connect { .. }) | None => {}
            Some(_) => self.finish(at_us, StepStatus::Disconnected, Vec::new()),
        }
        self.advance(at_us);
    }

    pub fn on_timer(&mut self, now_us: u64) {
        let Some(running) = self.running.as_ref() else {
            return;
        };
        if running.deadline_us.is_none_or(|d| d > now_us) {
            return;
        }
        match running.step {
            Step::Scan { .. } | Step::Wait { .. } => {
                self.finish(now_us, StepStatus::Ok, Vec::new())
            }
            _ => self.finish(now_us, StepStatus::Timeout, Vec::new()),
        }
        self.advance(now_us);
    }

    pub fn take_frames(&mut self) -> Vec<Vec<u8>> {
        self.link
            .as_mut()
            .map(|l| std::mem::take(&mut l.tx))
            .unwrap_or_default()
    }

    pub fn take_ll_requests(&mut self) -> Vec<LlRequest> {
        self.link
            .as_mut()
            .map(|l| std::mem::take(&mut l.ll))
            .unwrap_or_default()
    }

    /// `start` marks the first fragment of an L2CAP frame (Core Vol 6 Part B §2.4: `LLID` 0b10).
    pub fn on_fragment(&mut self, start: bool, data: &[u8], at_us: u64) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        if start {
            link.rx.clear();
        } else if link.rx.is_empty() {
            // A continuation with no start is dropped (Vol 3 Part A §7.2.1).
            return;
        }
        link.rx.extend_from_slice(data);
        if link.rx.len() < 4 {
            return;
        }
        let len = usize::from(le16(&link.rx, 0));
        if link.rx.len() < 4 + len {
            return;
        }
        let frame = std::mem::take(&mut link.rx);
        let channel = le16(&frame, 2);
        let payload = &frame[4..4 + len];
        match channel {
            cid::ATT => self.on_att(payload, at_us),
            cid::SIGNALING => self.on_signaling(payload),
            cid::SMP => self.on_smp(payload),
            _ => {}
        }
        self.advance(at_us);
    }

    fn send(&mut self, channel: u16, payload: &[u8]) {
        if let Some(link) = self.link.as_mut() {
            link.tx.push(l2cap(channel, payload));
        }
    }

    /// LE signaling (Core Vol 3 Part A §4): a valid connection parameter update (§4.20) is applied;
    /// other requests are rejected; responses need no answer.
    fn on_signaling(&mut self, pdu: &[u8]) {
        let (Some(&code), Some(&id)) = (pdu.first(), pdu.get(1)) else {
            return;
        };
        match code {
            0x12 if pdu.len() >= 12 => {
                let (min, max, latency, timeout) =
                    (le16(pdu, 4), le16(pdu, 6), le16(pdu, 8), le16(pdu, 10));
                let valid = (6..=3200).contains(&min)
                    && (6..=3200).contains(&max)
                    && min <= max
                    && latency <= 499
                    && (10..=3200).contains(&timeout)
                    && u32::from(timeout) * 4 > (1 + u32::from(latency)) * u32::from(max);
                let result: u16 = if valid { 0x0000 } else { 0x0001 };
                let [r0, r1] = result.to_le_bytes();
                self.send(cid::SIGNALING, &[0x13, id, 0x02, 0x00, r0, r1]);
                if valid && let Some(link) = self.link.as_mut() {
                    // Class C: the central takes the longest interval the peripheral allows.
                    link.ll.push(LlRequest::Update {
                        interval: max,
                        latency,
                        timeout,
                    });
                }
            }
            0x01 | 0x07 | 0x13 | 0x15 | 0x16 | 0x18 | 0x1A => {}
            _ => self.send(cid::SIGNALING, &[0x01, id, 0x02, 0x00, 0x00, 0x00]),
        }
    }

    /// SMP (Core Vol 3 Part H §3.5.5): pairing is not supported.
    fn on_smp(&mut self, pdu: &[u8]) {
        if pdu.first().is_some_and(|&code| code != 0x05) {
            self.send(cid::SMP, &[0x05, 0x05]);
        }
    }
}

/// The receive MTU the central offers when the peripheral exchanges MTU first, class C.
pub const CENTRAL_MTU: u16 = 247;

impl Central {
    fn on_att(&mut self, p: &[u8], at_us: u64) {
        let Some(&op) = p.first() else {
            return;
        };
        let handle = le16(p, 1);
        match op {
            att::HANDLE_VALUE_NTF | att::HANDLE_VALUE_IND if p.len() >= 3 => {
                let indication = op == att::HANDLE_VALUE_IND;
                if indication {
                    self.send(cid::ATT, &[att::HANDLE_VALUE_CFM]);
                }
                self.notifications.push(Notification {
                    at_us,
                    handle,
                    value: p[3..].to_vec(),
                    indication,
                });
                if self.notifications.len() > MAX_NOTIFICATIONS {
                    self.notifications.remove(0);
                }
                self.notified += 1;
                self.check_wait(at_us);
            }
            att::EXCHANGE_MTU_REQ => {
                let [lo, hi] = CENTRAL_MTU.to_le_bytes();
                self.send(cid::ATT, &[att::EXCHANGE_MTU_RSP, lo, hi]);
                if let Some(link) = self.link.as_mut() {
                    link.mtu = handle.clamp(DEFAULT_ATT_MTU, CENTRAL_MTU);
                }
            }
            att::FIND_INFORMATION_REQ
            | att::FIND_BY_TYPE_VALUE_REQ
            | att::READ_BY_TYPE_REQ
            | att::READ_BY_GROUP_TYPE_REQ => {
                self.send(
                    cid::ATT,
                    &att_error(op, handle, att::ERR_ATTRIBUTE_NOT_FOUND),
                );
            }
            att::READ_REQ | att::READ_BLOB_REQ | att::READ_MULTIPLE_REQ | att::WRITE_REQ => {
                self.send(cid::ATT, &att_error(op, handle, att::ERR_INVALID_HANDLE));
            }
            att::WRITE_CMD | att::HANDLE_VALUE_CFM => {}
            // Requests are the even opcodes below the notification, and 0x20 (Vol 3 Part F §3.4.8).
            _ if (op < att::HANDLE_VALUE_NTF && op % 2 == 0) || op == 0x20 => {
                self.send(cid::ATT, &att_error(op, 0, att::ERR_REQUEST_NOT_SUPPORTED));
            }
            _ => self.on_response(op, p, at_us),
        }
    }

    fn services_done(&mut self, now_us: u64) {
        match self.running.as_ref().map(|r| r.step.clone()) {
            Some(Step::DiscoverServices) => self.finish(now_us, StepStatus::Ok, Vec::new()),
            Some(Step::DiscoverCharacteristics { .. }) => match self.step_service() {
                Some(svc) => self.discover_characteristics(&svc, svc.start, now_us),
                None => self.finish(now_us, StepStatus::NotFound, Vec::new()),
            },
            _ => {}
        }
    }

    fn on_response(&mut self, op: u8, p: &[u8], now_us: u64) {
        let Some(link) = self.link.as_mut() else {
            return;
        };
        let Some(pending) = link.pending else {
            return;
        };
        let error = op == att::ERROR_RSP;
        if (error && p.get(1) != Some(&pending)) || (!error && op != pending + 1) {
            return;
        }
        link.pending = None;
        let Some(running) = self.running.clone() else {
            return;
        };
        let code = if error {
            p.get(4).copied().unwrap_or(0)
        } else {
            0
        };
        let not_found = error && code == att::ERR_ATTRIBUTE_NOT_FOUND;
        if error && !not_found {
            return self.finish(now_us, StepStatus::AttError(code), Vec::new());
        }
        match (&running.step, running.phase) {
            (Step::ExchangeMtu { .. }, _) if error => {
                self.finish(now_us, StepStatus::AttError(code), Vec::new())
            }
            (Step::ExchangeMtu { mtu }, _) => {
                let server = le16(p, 1).max(DEFAULT_ATT_MTU);
                if let Some(link) = self.link.as_mut() {
                    link.mtu = server.min((*mtu).max(DEFAULT_ATT_MTU));
                }
                self.finish(now_us, StepStatus::Ok, Vec::new());
            }
            (Step::DiscoverServices | Step::DiscoverCharacteristics { .. }, 0) => {
                if not_found {
                    return self.services_done(now_us);
                }
                let len = usize::from(p.get(1).copied().unwrap_or(0));
                let mut last = u16::MAX;
                if len >= 6 {
                    for e in p[2..].chunks_exact(len) {
                        let (start, end) = (le16(e, 0), le16(e, 2));
                        last = end;
                        if let Some(uuid) = Uuid::from_att(&e[4..])
                            && !self.services.iter().any(|s| s.start == start)
                        {
                            self.services.push(Service { start, end, uuid });
                        }
                    }
                }
                if last == u16::MAX || len < 6 {
                    self.services_done(now_us);
                } else {
                    self.discover_services(last + 1, now_us);
                }
            }
            (Step::DiscoverCharacteristics { .. }, 1) => {
                let Some(svc) = self.step_service() else {
                    return self.finish(now_us, StepStatus::NotFound, Vec::new());
                };
                if not_found {
                    return self.discover_descriptors(&svc, svc.start, now_us);
                }
                let len = usize::from(p.get(1).copied().unwrap_or(0));
                if len < 7 {
                    return self.discover_descriptors(&svc, svc.start, now_us);
                }
                let mut last = svc.end;
                for e in p[2..].chunks_exact(len) {
                    let declaration = le16(e, 0);
                    last = declaration;
                    let Some(uuid) = Uuid::from_att(&e[5..]) else {
                        continue;
                    };
                    self.characteristics
                        .retain(|c| c.declaration != declaration);
                    self.characteristics.push(Characteristic {
                        declaration,
                        properties: e[2],
                        value_handle: le16(e, 3),
                        uuid,
                        cccd: None,
                    });
                }
                self.characteristics.sort_by_key(|c| c.declaration);
                if last >= svc.end {
                    self.discover_descriptors(&svc, svc.start, now_us);
                } else {
                    self.discover_characteristics(&svc, last + 1, now_us);
                }
            }
            (Step::DiscoverCharacteristics { .. }, _) => {
                let Some(svc) = self.step_service() else {
                    return self.finish(now_us, StepStatus::NotFound, Vec::new());
                };
                let range_end = self
                    .descriptor_range(&svc, running.cursor)
                    .map_or(svc.end, |(_, end)| end);
                let mut next = range_end.checked_add(1);
                if !not_found {
                    let width = match p.get(1) {
                        Some(1) => 4,
                        Some(2) => 18,
                        _ => 0,
                    };
                    if width > 0 {
                        for e in p[2..].chunks_exact(width) {
                            let handle = le16(e, 0);
                            next = handle.checked_add(1);
                            if Uuid::from_att(&e[2..]) == Some(gatt::CCCD)
                                && let Some(c) = self
                                    .characteristics
                                    .iter_mut()
                                    .rev()
                                    .find(|c| c.value_handle < handle)
                                && c.cccd.is_none()
                            {
                                c.cccd = Some(handle);
                            }
                        }
                    }
                }
                match next {
                    Some(from) if from <= svc.end => self.discover_descriptors(&svc, from, now_us),
                    _ => self.finish(now_us, StepStatus::Ok, Vec::new()),
                }
            }
            (Step::Subscribe { .. } | Step::Write { .. }, _) if op == att::WRITE_RSP => {
                self.finish(now_us, StepStatus::Ok, Vec::new())
            }
            (Step::Read { .. }, _) if op == att::READ_RSP => {
                self.finish(now_us, StepStatus::Ok, p[1..].to_vec())
            }
            _ if error => self.finish(now_us, StepStatus::AttError(code), Vec::new()),
            _ => {}
        }
    }

    fn check_wait(&mut self, now_us: u64) {
        let Some(Step::WaitNotification {
            characteristic,
            contains,
            ..
        }) = self.running.as_ref().map(|r| r.step.clone())
        else {
            return;
        };
        let Some(c) = self.characteristic(characteristic) else {
            return self.finish(now_us, StepStatus::NotFound, Vec::new());
        };
        let first = self.notified - self.notifications.len() as u64;
        let skip = self.wait_from.saturating_sub(first) as usize;
        let seen: Vec<u8> = self
            .notifications
            .iter()
            .skip(skip)
            .filter(|n| n.handle == c.value_handle)
            .flat_map(|n| n.value.iter().copied())
            .collect();
        let new = self
            .notifications
            .iter()
            .skip(skip)
            .any(|n| n.handle == c.value_handle);
        let found = if contains.is_empty() {
            new
        } else {
            seen.windows(contains.len())
                .any(|w| w == contains.as_slice())
        };
        if found {
            self.wait_from = self.notified;
            self.finish(now_us, StepStatus::Ok, Vec::new());
        }
    }
}

// Snapshot codec and the journaled script form.

use super::codec::bad_tag;
use pemu_core::snap::snap_struct;

impl SnapValue for Uuid {
    fn snap_write(&self, out: &mut Vec<u8>) {
        self.0.snap_write(out);
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Uuid, SnapError> {
        Ok(Uuid(SnapValue::snap_read(r)?))
    }
}

impl SnapValue for Target {
    fn snap_write(&self, out: &mut Vec<u8>) {
        match self {
            Target::Any => out.push(0),
            Target::Address { address, random } => {
                out.push(1);
                address.snap_write(out);
                random.snap_write(out);
            }
            Target::Service(uuid) => {
                out.push(2);
                uuid.snap_write(out);
            }
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Target, SnapError> {
        Ok(match r.byte()? {
            0 => Target::Any,
            1 => Target::Address {
                address: SnapValue::snap_read(r)?,
                random: SnapValue::snap_read(r)?,
            },
            2 => Target::Service(SnapValue::snap_read(r)?),
            _ => return Err(bad_tag()),
        })
    }
}

impl SnapValue for Step {
    fn snap_write(&self, out: &mut Vec<u8>) {
        match self {
            Step::Scan { ms } => {
                out.push(0);
                ms.snap_write(out);
            }
            Step::Connect {
                target,
                interval,
                latency,
                timeout,
                within_ms,
            } => {
                out.push(1);
                target.snap_write(out);
                interval.snap_write(out);
                latency.snap_write(out);
                timeout.snap_write(out);
                within_ms.snap_write(out);
            }
            Step::ExchangeMtu { mtu } => {
                out.push(2);
                mtu.snap_write(out);
            }
            Step::DiscoverServices => out.push(3),
            Step::DiscoverCharacteristics { service } => {
                out.push(4);
                service.snap_write(out);
            }
            Step::Subscribe {
                characteristic,
                indicate,
            } => {
                out.push(5);
                characteristic.snap_write(out);
                indicate.snap_write(out);
            }
            Step::Write {
                characteristic,
                value,
                with_response,
            } => {
                out.push(6);
                characteristic.snap_write(out);
                value.snap_write(out);
                with_response.snap_write(out);
            }
            Step::Read { characteristic } => {
                out.push(7);
                characteristic.snap_write(out);
            }
            Step::WaitNotification {
                characteristic,
                contains,
                within_ms,
            } => {
                out.push(8);
                characteristic.snap_write(out);
                contains.snap_write(out);
                within_ms.snap_write(out);
            }
            Step::Wait { ms } => {
                out.push(9);
                ms.snap_write(out);
            }
            Step::Disconnect => out.push(10),
            Step::Capture { enabled } => {
                out.push(11);
                enabled.snap_write(out);
            }
            Step::CaptureSecrets { secrets } => {
                out.push(12);
                secrets.snap_write(out);
            }
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<Step, SnapError> {
        Ok(match r.byte()? {
            0 => Step::Scan {
                ms: SnapValue::snap_read(r)?,
            },
            1 => Step::Connect {
                target: SnapValue::snap_read(r)?,
                interval: SnapValue::snap_read(r)?,
                latency: SnapValue::snap_read(r)?,
                timeout: SnapValue::snap_read(r)?,
                within_ms: SnapValue::snap_read(r)?,
            },
            2 => Step::ExchangeMtu {
                mtu: SnapValue::snap_read(r)?,
            },
            3 => Step::DiscoverServices,
            4 => Step::DiscoverCharacteristics {
                service: SnapValue::snap_read(r)?,
            },
            5 => Step::Subscribe {
                characteristic: SnapValue::snap_read(r)?,
                indicate: SnapValue::snap_read(r)?,
            },
            6 => Step::Write {
                characteristic: SnapValue::snap_read(r)?,
                value: SnapValue::snap_read(r)?,
                with_response: SnapValue::snap_read(r)?,
            },
            7 => Step::Read {
                characteristic: SnapValue::snap_read(r)?,
            },
            8 => Step::WaitNotification {
                characteristic: SnapValue::snap_read(r)?,
                contains: SnapValue::snap_read(r)?,
                within_ms: SnapValue::snap_read(r)?,
            },
            9 => Step::Wait {
                ms: SnapValue::snap_read(r)?,
            },
            10 => Step::Disconnect,
            11 => Step::Capture {
                enabled: SnapValue::snap_read(r)?,
            },
            12 => Step::CaptureSecrets {
                secrets: SnapValue::snap_read(r)?,
            },
            _ => return Err(bad_tag()),
        })
    }
}

impl SnapValue for StepStatus {
    fn snap_write(&self, out: &mut Vec<u8>) {
        match self {
            StepStatus::Ok => out.push(0),
            StepStatus::Timeout => out.push(1),
            StepStatus::NotConnected => out.push(2),
            StepStatus::NotFound => out.push(3),
            StepStatus::AttError(code) => out.extend_from_slice(&[4, *code]),
            StepStatus::TooLong => out.push(5),
            StepStatus::Disconnected => out.push(6),
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<StepStatus, SnapError> {
        Ok(match r.byte()? {
            0 => StepStatus::Ok,
            1 => StepStatus::Timeout,
            2 => StepStatus::NotConnected,
            3 => StepStatus::NotFound,
            4 => StepStatus::AttError(r.byte()?),
            5 => StepStatus::TooLong,
            6 => StepStatus::Disconnected,
            _ => return Err(bad_tag()),
        })
    }
}

impl SnapValue for LlRequest {
    fn snap_write(&self, out: &mut Vec<u8>) {
        match self {
            LlRequest::Update {
                interval,
                latency,
                timeout,
            } => {
                out.push(0);
                interval.snap_write(out);
                latency.snap_write(out);
                timeout.snap_write(out);
            }
            LlRequest::Terminate { reason } => out.extend_from_slice(&[1, *reason]),
        }
    }

    fn snap_read(r: &mut SnapReader<'_>) -> Result<LlRequest, SnapError> {
        Ok(match r.byte()? {
            0 => LlRequest::Update {
                interval: SnapValue::snap_read(r)?,
                latency: SnapValue::snap_read(r)?,
                timeout: SnapValue::snap_read(r)?,
            },
            1 => LlRequest::Terminate { reason: r.byte()? },
            _ => return Err(bad_tag()),
        })
    }
}

snap_struct!(StepResult {
    index,
    at_us,
    status,
    value
});
snap_struct!(AdvReport {
    at_us,
    channel,
    pdu_type,
    address,
    random,
    data,
    scan_response
});
snap_struct!(Service { start, end, uuid });
snap_struct!(Characteristic {
    declaration,
    properties,
    value_handle,
    uuid,
    cccd
});
snap_struct!(Notification {
    at_us,
    handle,
    value,
    indication
});
snap_struct!(Running {
    index,
    step,
    deadline_us,
    phase,
    cursor,
    scan_requested
});
snap_struct!(CentralLink {
    handle,
    mtu,
    rx,
    tx,
    pending,
    ll,
    terminating
});
snap_struct!(Central {
    next_index,
    queue,
    running,
    results,
    reports,
    link,
    services,
    characteristics,
    notifications,
    notified,
    wait_from,
    refused_steps
});

/// The version byte a journaled script starts with. A new tag at the end of the tag space does
/// not move it; changing the payload of an existing tag does.
pub const SCRIPT_VERSION: u8 = 1;

/// The script as `EnvChange::BleCentral` journals it: [`SCRIPT_VERSION`], then the steps.
pub fn encode_script(steps: &[Step]) -> Vec<u8> {
    let mut out = vec![SCRIPT_VERSION];
    steps.to_vec().snap_write(&mut out);
    out
}

pub fn decode_script(bytes: &[u8]) -> Result<Vec<Step>, String> {
    let (&version, rest) = bytes.split_first().ok_or("an empty BLE central script")?;
    if version != SCRIPT_VERSION {
        return Err(format!(
            "BLE central script version {version} is not {SCRIPT_VERSION}"
        ));
    }
    let mut r = SnapReader::new(rest, "ble central script");
    let steps = Vec::<Step>::snap_read(&mut r).map_err(|e| format!("BLE central script: {e:?}"))?;
    if !r.is_empty() {
        return Err("BLE central script has trailing bytes".to_string());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "a BLE central script holds at most {MAX_STEPS} steps"
        ));
    }
    let long = steps.iter().any(|s| match s {
        Step::Write { value, .. } => value.len() > MAX_VALUE,
        Step::WaitNotification { contains, .. } => contains.len() > MAX_VALUE,
        _ => false,
    });
    if long {
        return Err(format!(
            "a BLE central script value holds at most {MAX_VALUE} bytes"
        ));
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VENDOR: &str = "12D4FA08-7418-48FA-A95A-B43A2E669E55";
    const EVENTS: &str = "12D4FA09-7418-48FA-A95A-B43A2E669E55";
    const COMMANDS: &str = "12D4FA0A-7418-48FA-A95A-B43A2E669E55";

    fn uuid(text: &str) -> Uuid {
        Uuid::parse(text).expect("a UUID")
    }

    /// A GATT server answering one entry per discovery response, so every procedure has to continue
    /// from the last handle.
    struct Server {
        attrs: Vec<(u16, Uuid, Vec<u8>)>,
        mtu: u16,
        writes: Vec<(u16, Vec<u8>)>,
    }

    impl Server {
        fn passport_keys() -> Server {
            let decl = |props: u8, value: u16, u: Uuid| {
                let mut v = vec![props];
                v.extend_from_slice(&value.to_le_bytes());
                v.extend_from_slice(&u.to_att());
                v
            };
            Server {
                attrs: vec![
                    (1, gatt::PRIMARY_SERVICE, vec![0x00, 0x18]),
                    (
                        2,
                        gatt::CHARACTERISTIC,
                        decl(0x02, 3, Uuid::from_u16(0x2A00)),
                    ),
                    (3, Uuid::from_u16(0x2A00), b"Passport Keys".to_vec()),
                    (4, gatt::PRIMARY_SERVICE, uuid(VENDOR).0.to_vec()),
                    (5, gatt::CHARACTERISTIC, decl(0x10, 6, uuid(EVENTS))),
                    (6, uuid(EVENTS), Vec::new()),
                    (7, gatt::CCCD, vec![0, 0]),
                    (8, gatt::CHARACTERISTIC, decl(0x0C, 9, uuid(COMMANDS))),
                    (9, uuid(COMMANDS), Vec::new()),
                ],
                mtu: 256,
                writes: Vec::new(),
            }
        }

        fn end_of_group(&self, start: u16) -> u16 {
            self.attrs
                .iter()
                .find(|a| a.0 > start && a.1 == gatt::PRIMARY_SERVICE)
                .map_or(0xFFFF, |a| a.0 - 1)
        }

        fn answer(&mut self, p: &[u8]) -> Option<Vec<u8>> {
            let (start, end) = (le16(p, 1), le16(p, 3));
            let in_range = |h: u16| h >= start && h <= end;
            let not_found = || Some(att_error(p[0], start, att::ERR_ATTRIBUTE_NOT_FOUND));
            match p[0] {
                att::EXCHANGE_MTU_REQ => {
                    let mut r = vec![att::EXCHANGE_MTU_RSP];
                    r.extend_from_slice(&self.mtu.to_le_bytes());
                    Some(r)
                }
                att::READ_BY_GROUP_TYPE_REQ => {
                    let a = self
                        .attrs
                        .iter()
                        .find(|a| in_range(a.0) && a.1 == gatt::PRIMARY_SERVICE)?
                        .clone();
                    let mut r = vec![att::READ_BY_GROUP_TYPE_RSP, (4 + a.2.len()) as u8];
                    r.extend_from_slice(&a.0.to_le_bytes());
                    r.extend_from_slice(&self.end_of_group(a.0).to_le_bytes());
                    r.extend_from_slice(&a.2);
                    Some(r)
                }
                att::READ_BY_TYPE_REQ => {
                    let want = Uuid::from_att(&p[5..])?;
                    let Some(a) = self.attrs.iter().find(|a| in_range(a.0) && a.1 == want) else {
                        return not_found();
                    };
                    let mut r = vec![att::READ_BY_TYPE_RSP, (2 + a.2.len()) as u8];
                    r.extend_from_slice(&a.0.to_le_bytes());
                    r.extend_from_slice(&a.2);
                    Some(r)
                }
                att::FIND_INFORMATION_REQ => {
                    let Some(a) = self.attrs.iter().find(|a| in_range(a.0)) else {
                        return not_found();
                    };
                    let form = a.1.to_att();
                    let mut r = vec![
                        att::FIND_INFORMATION_RSP,
                        if form.len() == 2 { 1 } else { 2 },
                    ];
                    r.extend_from_slice(&a.0.to_le_bytes());
                    r.extend_from_slice(&form);
                    Some(r)
                }
                att::WRITE_REQ | att::WRITE_CMD => {
                    self.writes.push((le16(p, 1), p[3..].to_vec()));
                    (p[0] == att::WRITE_REQ).then(|| vec![att::WRITE_RSP])
                }
                att::READ_REQ => {
                    let a = self.attrs.iter().find(|a| a.0 == le16(p, 1))?;
                    let mut r = vec![att::READ_RSP];
                    r.extend_from_slice(&a.2);
                    Some(r)
                }
                _ => None,
            }
        }
    }

    fn deliver(c: &mut Central, frame: &[u8], at: u64) {
        for (i, chunk) in frame.chunks(27).enumerate() {
            c.on_fragment(i == 0, chunk, at);
        }
    }

    fn pump(c: &mut Central, s: &mut Server, mut at: u64, limit: usize) -> u64 {
        for _ in 0..limit {
            let frames = c.take_frames();
            if frames.is_empty() && c.running.is_some() {
                return at;
            }
            if frames.is_empty() {
                return at;
            }
            at += 30_000;
            for f in frames {
                assert_eq!(le16(&f, 2), cid::ATT);
                if let Some(rsp) = s.answer(&f[4..]) {
                    deliver(c, &l2cap(cid::ATT, &rsp), at);
                }
            }
        }
        at
    }

    fn connected() -> Central {
        let mut c = Central::default();
        c.push_steps(
            vec![Step::Connect {
                target: Target::Any,
                interval: 24,
                latency: 0,
                timeout: 400,
                within_ms: 1_000,
            }],
            0,
        );
        c.on_connected(1, 100);
        c
    }

    #[test]
    fn the_gatt_exchange_discovers_subscribes_writes_and_waits_for_notifications() {
        let mut c = connected();
        let mut s = Server::passport_keys();
        c.push_steps(
            vec![
                Step::ExchangeMtu { mtu: 247 },
                Step::DiscoverServices,
                Step::DiscoverCharacteristics {
                    service: uuid(VENDOR),
                },
                Step::Subscribe {
                    characteristic: uuid(EVENTS),
                    indicate: false,
                },
                Step::Write {
                    characteristic: uuid(COMMANDS),
                    value: b"{\"cmd\":\"hello\"}\n".to_vec(),
                    with_response: true,
                },
                Step::WaitNotification {
                    characteristic: uuid(EVENTS),
                    contains: b"\"t\":\"hello\"".to_vec(),
                    within_ms: 2_000,
                },
            ],
            200,
        );
        let at = pump(&mut c, &mut s, 200, 100);
        assert_eq!(c.link.as_ref().map(|l| l.mtu), Some(247));
        assert_eq!(
            c.services
                .iter()
                .map(|s| (s.start, s.end))
                .collect::<Vec<_>>(),
            [(1, 3), (4, 0xFFFF)]
        );
        let events = c.characteristic(uuid(EVENTS)).expect("discovered");
        assert_eq!((events.value_handle, events.cccd), (6, Some(7)));
        let commands = c.characteristic(uuid(COMMANDS)).expect("discovered");
        assert_eq!((commands.value_handle, commands.cccd), (9, None));
        assert_eq!(
            s.writes,
            [
                (7, vec![0x01, 0x00]),
                (9, b"{\"cmd\":\"hello\"}\n".to_vec())
            ]
        );
        // The wait runs until the notification arrives, split in two values.
        assert!(matches!(
            c.running.as_ref().map(|r| &r.step),
            Some(Step::WaitNotification { .. })
        ));
        deliver(
            &mut c,
            &l2cap(
                cid::ATT,
                &[att::HANDLE_VALUE_NTF, 6, 0, b'{', b'"', b't', b'"'],
            ),
            at,
        );
        assert!(c.running.is_some(), "not yet");
        let mut rest = vec![att::HANDLE_VALUE_NTF, 6, 0];
        rest.extend_from_slice(b":\"hello\",\"fw\":\"passport-keys\"}\n");
        deliver(&mut c, &l2cap(cid::ATT, &rest), at + 30_000);
        assert!(c.running.is_none());
        assert_eq!(c.results.len(), 7);
        assert!(
            c.results.iter().all(|r| r.status == StepStatus::Ok),
            "{:?}",
            c.results
        );
        assert_eq!(c.notified, 2);
        // A later wait does not match the bytes an earlier one consumed.
        c.push_steps(
            vec![Step::WaitNotification {
                characteristic: uuid(EVENTS),
                contains: b"hello".to_vec(),
                within_ms: 100,
            }],
            at + 60_000,
        );
        assert!(c.running.is_some());
        c.on_timer(at + 60_000 + 100_000);
        assert_eq!(
            c.results.last().map(|r| r.status),
            Some(StepStatus::Timeout)
        );
    }

    #[test]
    fn a_read_returns_its_value_and_a_missing_attribute_is_not_found() {
        let mut c = connected();
        let mut s = Server::passport_keys();
        c.push_steps(
            vec![
                Step::DiscoverCharacteristics {
                    service: Uuid::from_u16(0x1800),
                },
                Step::Read {
                    characteristic: Uuid::from_u16(0x2A00),
                },
                Step::DiscoverCharacteristics {
                    service: Uuid::from_u16(0x180F),
                },
                Step::Subscribe {
                    characteristic: Uuid::from_u16(0x2A00),
                    indicate: true,
                },
            ],
            0,
        );
        pump(&mut c, &mut s, 0, 100);
        let statuses: Vec<_> = c.results.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            [
                StepStatus::Ok,
                StepStatus::Ok,
                StepStatus::Ok,
                StepStatus::NotFound,
                StepStatus::NotFound
            ]
        );
        assert_eq!(c.results[2].value, b"Passport Keys");
    }

    #[test]
    fn steps_without_a_link_or_a_response_end_with_their_reason() {
        let mut c = Central::default();
        c.push_steps(
            vec![
                Step::DiscoverServices,
                Step::Connect {
                    target: Target::Any,
                    interval: 24,
                    latency: 0,
                    timeout: 400,
                    within_ms: 500,
                },
            ],
            0,
        );
        assert_eq!(c.results[0].status, StepStatus::NotConnected);
        assert_eq!(c.timer_us(), Some(500_000));
        c.on_timer(499_999);
        assert!(c.running.is_some());
        c.on_timer(500_000);
        assert_eq!(c.results[1].status, StepStatus::Timeout);

        let mut c = connected();
        c.characteristics.push(Characteristic {
            declaration: 8,
            properties: 0x0C,
            value_handle: 9,
            uuid: uuid(COMMANDS),
            cccd: None,
        });
        c.push_steps(
            vec![
                Step::Write {
                    characteristic: uuid(COMMANDS),
                    value: vec![0; 21],
                    with_response: true,
                },
                Step::Write {
                    characteristic: uuid(COMMANDS),
                    value: vec![0; 20],
                    with_response: true,
                },
            ],
            1_000,
        );
        assert_eq!(c.results[1].status, StepStatus::TooLong);
        assert_eq!(c.timer_us(), Some(1_000 + ATT_TIMEOUT_US));
        c.on_timer(1_000 + ATT_TIMEOUT_US);
        assert_eq!(c.results[2].status, StepStatus::Timeout);
        deliver(
            &mut c,
            &l2cap(cid::ATT, &[att::WRITE_RSP]),
            1_000 + ATT_TIMEOUT_US,
        );
        assert_eq!(c.results.len(), 3);
        c.push_steps(
            vec![Step::Write {
                characteristic: uuid(COMMANDS),
                value: vec![1],
                with_response: true,
            }],
            40_000_000,
        );
        c.take_frames();
        deliver(
            &mut c,
            &l2cap(cid::ATT, &att_error(att::WRITE_REQ, 9, 0x03)),
            40_000_000,
        );
        assert_eq!(c.results[3].status, StepStatus::AttError(0x03));
        c.push_steps(
            vec![Step::Read {
                characteristic: uuid(COMMANDS),
            }],
            41_000_000,
        );
        c.on_disconnected(41_000_001);
        assert_eq!(c.results[4].status, StepStatus::Disconnected);
        assert_eq!(c.link, None);
    }

    #[test]
    fn requests_from_the_peripheral_are_answered() {
        let mut c = connected();
        deliver(
            &mut c,
            &l2cap(cid::ATT, &[att::EXCHANGE_MTU_REQ, 0x00, 0x02]),
            0,
        );
        deliver(
            &mut c,
            &l2cap(
                cid::ATT,
                &[att::READ_BY_TYPE_REQ, 1, 0, 0xFF, 0xFF, 0x00, 0x2A],
            ),
            0,
        );
        deliver(&mut c, &l2cap(cid::ATT, &[att::READ_REQ, 3, 0]), 0);
        deliver(&mut c, &l2cap(cid::ATT, &[0x16, 3, 0, 0, 0]), 0);
        deliver(
            &mut c,
            &l2cap(cid::ATT, &[att::HANDLE_VALUE_IND, 6, 0, 1]),
            0,
        );
        // Connection Parameter Update Request: 15 to 30 ms, latency 4, 4 s.
        deliver(
            &mut c,
            &l2cap(
                cid::SIGNALING,
                &[0x12, 7, 8, 0, 12, 0, 24, 0, 4, 0, 0x90, 0x01],
            ),
            0,
        );
        deliver(&mut c, &l2cap(cid::SIGNALING, &[0x14, 9, 0, 0]), 0);
        deliver(&mut c, &l2cap(cid::SMP, &[0x0B, 0x01]), 0);
        deliver(&mut c, &l2cap(cid::SMP, &[0x05, 0x05]), 0);
        let frames = c.take_frames();
        assert_eq!(
            frames,
            [
                l2cap(cid::ATT, &[att::EXCHANGE_MTU_RSP, 247, 0]),
                l2cap(cid::ATT, &att_error(att::READ_BY_TYPE_REQ, 1, 0x0A)),
                l2cap(cid::ATT, &att_error(att::READ_REQ, 3, 0x01)),
                l2cap(cid::ATT, &att_error(0x16, 0, 0x06)),
                l2cap(cid::ATT, &[att::HANDLE_VALUE_CFM]),
                l2cap(cid::SIGNALING, &[0x13, 7, 2, 0, 0, 0]),
                l2cap(cid::SIGNALING, &[0x01, 9, 2, 0, 0, 0]),
                l2cap(cid::SMP, &[0x05, 0x05]),
            ]
        );
        assert_eq!(c.link.as_ref().map(|l| l.mtu), Some(247));
        assert_eq!(
            c.take_ll_requests(),
            [LlRequest::Update {
                interval: 24,
                latency: 4,
                timeout: 400
            }]
        );
        assert!(c.notifications[0].indication);
    }

    #[test]
    fn advertisers_are_scanned_once_per_step_and_connected_by_service() {
        use crate::ble::air::pdu;
        let addr = [6, 5, 4, 3, 2, 1];
        let mut adv = vec![0x02, 0x01, 0x06, 0x11, 0x07];
        adv.extend_from_slice(&uuid(VENDOR).0);
        let mut c = Central::default();
        assert_eq!(
            c.on_advertising(0, 37, pdu::ADV_IND, addr, false, &adv, false),
            AdvAction::None
        );
        c.push_steps(vec![Step::Scan { ms: 100 }], 0);
        assert_eq!(
            c.on_advertising(10, 37, pdu::ADV_IND, addr, false, &adv, false),
            AdvAction::ScanRequest
        );
        assert_eq!(
            c.on_advertising(20, 38, pdu::ADV_IND, addr, false, &adv, false),
            AdvAction::None
        );
        c.on_scan_response(30, addr, false, b"\x0e\x09Passport Keys");
        c.on_timer(100_000);
        assert_eq!(c.results[0].status, StepStatus::Ok);
        assert_eq!(c.reports.len(), 1);
        assert_eq!(
            c.reports[0].scan_response.as_deref(),
            Some(&b"\x0e\x09Passport Keys"[..])
        );
        let connect = |target| Step::Connect {
            target,
            interval: 24,
            latency: 0,
            timeout: 400,
            within_ms: 1_000,
        };
        c.push_steps(
            vec![connect(Target::Service(Uuid::from_u16(0x180F)))],
            200_000,
        );
        assert_eq!(
            c.on_advertising(210_000, 37, pdu::ADV_IND, addr, false, &adv, false),
            AdvAction::None
        );
        c.on_timer(1_200_000);
        c.push_steps(vec![connect(Target::Service(uuid(VENDOR)))], 1_300_000);
        assert_eq!(
            c.on_advertising(1_310_000, 37, pdu::ADV_SCAN_IND, addr, false, &adv, false),
            AdvAction::None,
            "not connectable"
        );
        assert_eq!(
            c.on_advertising(1_320_000, 37, pdu::ADV_IND, addr, false, &adv, false),
            AdvAction::Connect {
                interval: 24,
                latency: 0,
                timeout: 400
            }
        );
        c.on_connected(1, 1_320_300);
        assert_eq!(
            c.results.iter().map(|r| r.status).collect::<Vec<_>>(),
            [StepStatus::Ok, StepStatus::Timeout, StepStatus::Ok]
        );
    }

    #[test]
    fn scripts_round_trip_and_bad_scripts_are_refused() {
        let steps = vec![
            Step::Scan { ms: 1 },
            Step::Connect {
                target: Target::Address {
                    address: [1, 2, 3, 4, 5, 6],
                    random: true,
                },
                interval: 6,
                latency: 1,
                timeout: 100,
                within_ms: 9,
            },
            Step::ExchangeMtu { mtu: 247 },
            Step::DiscoverServices,
            Step::DiscoverCharacteristics {
                service: uuid(VENDOR),
            },
            Step::Subscribe {
                characteristic: uuid(EVENTS),
                indicate: true,
            },
            Step::Write {
                characteristic: uuid(COMMANDS),
                value: b"x".to_vec(),
                with_response: false,
            },
            Step::Read {
                characteristic: Uuid::from_u16(0x2A00),
            },
            Step::WaitNotification {
                characteristic: uuid(EVENTS),
                contains: b"y".to_vec(),
                within_ms: 3,
            },
            Step::Wait { ms: 4 },
            Step::Capture { enabled: false },
            Step::CaptureSecrets { secrets: true },
            Step::Disconnect,
            Step::Connect {
                target: Target::Service(uuid(VENDOR)),
                interval: 6,
                latency: 0,
                timeout: 10,
                within_ms: 1,
            },
        ];
        let bytes = encode_script(&steps);
        assert_eq!(decode_script(&bytes), Ok(steps));
        assert!(decode_script(&[]).is_err());
        assert!(decode_script(&[2, 0]).is_err());
        assert!(decode_script(&[SCRIPT_VERSION, 1, 99]).is_err());
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_script(&trailing).is_err());
        let long = encode_script(&[Step::Write {
            characteristic: uuid(COMMANDS),
            value: vec![0; MAX_VALUE + 1],
            with_response: true,
        }]);
        assert!(decode_script(&long).is_err());
        assert!(decode_script(&encode_script(&vec![Step::Disconnect; MAX_STEPS + 1])).is_err());
    }

    #[test]
    fn uuids_parse_most_significant_first_and_16_bit_forms_are_short() {
        let u = uuid(VENDOR);
        assert_eq!(u.0[0], 0x55);
        assert_eq!(u.0[15], 0x12);
        assert_eq!(u.to_att().len(), 16);
        assert_eq!(Uuid::parse("2902"), Some(gatt::CCCD));
        assert_eq!(gatt::CCCD.to_att(), [0x02, 0x29]);
        assert_eq!(Uuid::parse("12D4FA08-7418-48FA-A95A-B43A2E669E5"), None);
        let mut adv = vec![0x03, 0x03, 0x0F, 0x18];
        adv.extend_from_slice(&[0x11, 0x07]);
        adv.extend_from_slice(&u.0);
        assert!(ad_lists_service(&adv, Uuid::from_u16(0x180F)));
        assert!(ad_lists_service(&adv, u));
        assert!(!ad_lists_service(&adv, gatt::CCCD));
        assert!(!ad_lists_service(
            &[0x05, 0x03, 0x0F],
            Uuid::from_u16(0x180F)
        ));
    }

    #[test]
    fn the_central_state_round_trips_through_its_snapshot_codec() {
        let mut c = connected();
        let mut s = Server::passport_keys();
        c.push_steps(
            vec![
                Step::DiscoverCharacteristics {
                    service: uuid(VENDOR),
                },
                Step::Wait { ms: 5 },
            ],
            0,
        );
        pump(&mut c, &mut s, 0, 3);
        c.link.as_mut().expect("link").rx = vec![9, 0, 4];
        let mut bytes = Vec::new();
        c.snap_write(&mut bytes);
        let mut r = SnapReader::new(&bytes, "test");
        assert_eq!(Central::snap_read(&mut r), Ok(c));
        assert!(r.is_empty());
    }
}
