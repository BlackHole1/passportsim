//! The WebSocket protocol of `serve`, at `/v1/instances/{id}/ws`: the codec, the backpressure
//! policy and the socket pump. [`crate::hub`] is the producer.
//!
//! Text frames are JSON-RPC (registry calls, `subscribe`, and pushed `event.*` notifications).
//! Binary frames are exactly one of `FRM1` (28-byte header, a dirty panel rectangle), `AUD1`
//! (24 bytes, speaker path, server to client) or `MIC1` (same header, mic path, client to server),
//! so a reader dispatches on the first four bytes. Headers are little-endian and packed by hand,
//! because a `repr(C)` layout is a promise about this build and the wire format is a promise to a
//! browser.
//!
//! Backpressure never touches virtual time: a client that falls behind gets the union of the
//! rectangles it missed and a dropped-frame count, never a growing queue ([`Session`]).

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use crate::http::Server;

pub const SUBPROTOCOL: &str = "passportsim.v1";

pub const FRM1: [u8; 4] = *b"FRM1";
pub const AUD1: [u8; 4] = *b"AUD1";
pub const MIC1: [u8; 4] = *b"MIC1";

pub const FRAME_HEADER_BYTES: usize = 28;
pub const AUDIO_HEADER_BYTES: usize = 24;

/// `flags` bit 0: this frame is the whole panel, not a dirty rectangle.
pub const FLAG_FULL_FRAME: u8 = 1 << 0;
pub const FLAG_DISPLAY_ON: u8 = 1 << 1;
pub const FLAG_INVERTED: u8 = 1 << 2;
pub const FLAG_SLEEP_IN: u8 = 1 << 3;
/// `flags` bit 4: the glass shows the bitwise complement of the payload
/// (`FramePort::glass_complement`, i.e. `inverted != invon_shows_ram`).
///
/// [`FLAG_INVERTED`] is only the ST7789 INVON state and never a drawing rule: the official menu
/// sends INVON at init and its sky 0x145D still shows as 0x145D. A consumer complements exactly
/// when this bit is set.
pub const FLAG_GLASS_COMPLEMENT: u8 = 1 << 4;
/// `flags` bit 5: the panel rail is on. It tells a panel sent DISPOFF (memory kept) from one whose
/// rail is down (memory lost, repaint needed). A consumer draws black unless this bit and
/// [`FLAG_DISPLAY_ON`] are set and [`FLAG_SLEEP_IN`] is clear.
pub const FLAG_POWERED: u8 = 1 << 5;

/// The `FRM1` `flags` byte of a frame port. Bit 0 is left clear: [`Session::take`] sets it when the
/// union covers the panel.
pub fn frame_flags(port: &pemu_core::hostio::FramePort) -> u8 {
    let mut flags = 0;
    if port.display_on() {
        flags |= FLAG_DISPLAY_ON;
    }
    if port.inverted() {
        flags |= FLAG_INVERTED;
    }
    if port.sleeping() {
        flags |= FLAG_SLEEP_IN;
    }
    if port.glass_complement() {
        flags |= FLAG_GLASS_COMPLEMENT;
    }
    if port.powered() {
        flags |= FLAG_POWERED;
    }
    flags
}

/// The RGB565LE payload of `rect` from a row-major 240-wide raw view. Pixels travel as panel
/// memory holds them; the glass rule travels in the flags. A pixel outside `pixels` reads as 0.
pub fn frame_payload(pixels: &[u16], rect: Rect) -> Vec<u8> {
    let width = pemu_core::hostio::FRAME_WIDTH;
    let mut out = Vec::with_capacity(rect.pixels() * PixelFormat::Rgb565Le.bytes());
    for row in usize::from(rect.y)..usize::from(rect.y) + usize::from(rect.h) {
        for col in usize::from(rect.x)..usize::from(rect.x) + usize::from(rect.w) {
            let pixel = pixels.get(row * width + col).copied().unwrap_or(0);
            out.extend_from_slice(&pixel.to_le_bytes());
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// 1: RGB565 little-endian, the panel's native format.
    Rgb565Le,
    /// 2: RGB888.
    Rgb888,
}

impl PixelFormat {
    pub fn code(self) -> u8 {
        match self {
            PixelFormat::Rgb565Le => 1,
            PixelFormat::Rgb888 => 2,
        }
    }

    pub fn from_code(code: u8) -> Option<PixelFormat> {
        match code {
            1 => Some(PixelFormat::Rgb565Le),
            2 => Some(PixelFormat::Rgb888),
            _ => None,
        }
    }

    pub fn bytes(self) -> usize {
        match self {
            PixelFormat::Rgb565Le => 2,
            PixelFormat::Rgb888 => 3,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// The smallest rectangle holding both, which is what a client that fell behind receives.
    pub fn union(self, other: Rect) -> Rect {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = (self.x + self.w).max(other.x + other.w);
        let bottom = (self.y + self.h).max(other.y + other.h);
        Rect {
            x,
            y,
            w: right - x,
            h: bottom - y,
        }
    }

    pub fn pixels(self) -> usize {
        usize::from(self.w) * usize::from(self.h)
    }
}

/// The 28-byte `FRM1` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameHeader {
    pub frame_no: u32,
    /// Virtual time of the frame in nanoseconds.
    pub vt_ns: u64,
    pub rect: Rect,
    pub format: PixelFormat,
    pub flags: u8,
    /// LEDC duty of the backlight, 0 to 1023.
    pub backlight: u16,
}

impl FrameHeader {
    pub fn payload_bytes(&self) -> usize {
        self.rect.pixels() * self.format.bytes()
    }

    pub fn to_bytes(&self) -> [u8; FRAME_HEADER_BYTES] {
        let mut out = [0u8; FRAME_HEADER_BYTES];
        out[0..4].copy_from_slice(&FRM1);
        out[4..8].copy_from_slice(&self.frame_no.to_le_bytes());
        out[8..16].copy_from_slice(&self.vt_ns.to_le_bytes());
        out[16..18].copy_from_slice(&self.rect.x.to_le_bytes());
        out[18..20].copy_from_slice(&self.rect.y.to_le_bytes());
        out[20..22].copy_from_slice(&self.rect.w.to_le_bytes());
        out[22..24].copy_from_slice(&self.rect.h.to_le_bytes());
        out[24] = self.format.code();
        out[25] = self.flags;
        out[26..28].copy_from_slice(&self.backlight.to_le_bytes());
        out
    }
}

/// The 24-byte `AUD1` or `MIC1` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioHeader {
    pub magic: [u8; 4],
    /// Virtual time of the first frame, in nanoseconds.
    pub vt_ns: u64,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits: u8,
    /// 0 is the I2S TX (speaker) path, 1 the I2S RX (mic) path.
    pub dir: u8,
    pub n_frames: u32,
}

impl AudioHeader {
    pub fn payload_bytes(&self) -> usize {
        self.n_frames as usize * usize::from(self.channels) * usize::from(self.bits.div_ceil(8))
    }

    /// The header as its 24 wire bytes. Byte 19 is reserved and stays zero.
    pub fn to_bytes(&self) -> [u8; AUDIO_HEADER_BYTES] {
        let mut out = [0u8; AUDIO_HEADER_BYTES];
        out[0..4].copy_from_slice(&self.magic);
        out[4..12].copy_from_slice(&self.vt_ns.to_le_bytes());
        out[12..16].copy_from_slice(&self.sample_rate.to_le_bytes());
        out[16] = self.channels;
        out[17] = self.bits;
        out[18] = self.dir;
        out[19] = 0;
        out[20..24].copy_from_slice(&self.n_frames.to_le_bytes());
        out
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooShort {
        got: usize,
        /// Bytes the header needs.
        want: usize,
    },
    UnknownMagic([u8; 4]),
    UnknownFormat(u8),
    PayloadLength {
        got: usize,
        /// Bytes the header describes.
        want: usize,
    },
    WrongDirection {
        /// The magic that arrived.
        magic: [u8; 4],
    },
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::TooShort { got, want } => {
                write!(
                    f,
                    "a binary frame of {got} bytes is shorter than its {want}-byte header"
                )
            }
            FrameError::UnknownMagic(magic) => write!(
                f,
                "`{}` is not a binary frame of this protocol; the three are FRM1, AUD1 and MIC1",
                String::from_utf8_lossy(magic)
            ),
            FrameError::UnknownFormat(code) => {
                write!(f, "pixel format {code} is not 1 (RGB565LE) or 2 (RGB888)")
            }
            FrameError::PayloadLength { got, want } => write!(
                f,
                "the header describes {want} payload bytes and the frame carries {got}"
            ),
            FrameError::WrongDirection { magic } => write!(
                f,
                "`{}` travels the other way on this socket",
                String::from_utf8_lossy(magic)
            ),
        }
    }
}

impl std::error::Error for FrameError {}

pub fn encode_frame(header: &FrameHeader, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let want = header.payload_bytes();
    if payload.len() != want {
        return Err(FrameError::PayloadLength {
            got: payload.len(),
            want,
        });
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_frame(bytes: &[u8]) -> Result<(FrameHeader, &[u8]), FrameError> {
    if bytes.len() < FRAME_HEADER_BYTES {
        return Err(FrameError::TooShort {
            got: bytes.len(),
            want: FRAME_HEADER_BYTES,
        });
    }
    let magic: [u8; 4] = bytes[0..4].try_into().expect("four bytes");
    if magic != FRM1 {
        return Err(FrameError::UnknownMagic(magic));
    }
    let format = PixelFormat::from_code(bytes[24]).ok_or(FrameError::UnknownFormat(bytes[24]))?;
    let header = FrameHeader {
        frame_no: u32::from_le_bytes(bytes[4..8].try_into().expect("four bytes")),
        vt_ns: u64::from_le_bytes(bytes[8..16].try_into().expect("eight bytes")),
        rect: Rect {
            x: u16::from_le_bytes(bytes[16..18].try_into().expect("two bytes")),
            y: u16::from_le_bytes(bytes[18..20].try_into().expect("two bytes")),
            w: u16::from_le_bytes(bytes[20..22].try_into().expect("two bytes")),
            h: u16::from_le_bytes(bytes[22..24].try_into().expect("two bytes")),
        },
        format,
        flags: bytes[25],
        backlight: u16::from_le_bytes(bytes[26..28].try_into().expect("two bytes")),
    };
    let payload = &bytes[FRAME_HEADER_BYTES..];
    let want = header.payload_bytes();
    if payload.len() != want {
        return Err(FrameError::PayloadLength {
            got: payload.len(),
            want,
        });
    }
    Ok((header, payload))
}

pub fn encode_audio(header: &AudioHeader, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if header.magic != AUD1 && header.magic != MIC1 {
        return Err(FrameError::UnknownMagic(header.magic));
    }
    let want = header.payload_bytes();
    if payload.len() != want {
        return Err(FrameError::PayloadLength {
            got: payload.len(),
            want,
        });
    }
    let mut out = Vec::with_capacity(AUDIO_HEADER_BYTES + payload.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

pub fn decode_audio(bytes: &[u8]) -> Result<(AudioHeader, &[u8]), FrameError> {
    if bytes.len() < AUDIO_HEADER_BYTES {
        return Err(FrameError::TooShort {
            got: bytes.len(),
            want: AUDIO_HEADER_BYTES,
        });
    }
    let magic: [u8; 4] = bytes[0..4].try_into().expect("four bytes");
    if magic != AUD1 && magic != MIC1 {
        return Err(FrameError::UnknownMagic(magic));
    }
    let header = AudioHeader {
        magic,
        vt_ns: u64::from_le_bytes(bytes[4..12].try_into().expect("eight bytes")),
        sample_rate: u32::from_le_bytes(bytes[12..16].try_into().expect("four bytes")),
        channels: bytes[16],
        bits: bytes[17],
        dir: bytes[18],
        n_frames: u32::from_le_bytes(bytes[20..24].try_into().expect("four bytes")),
    };
    let payload = &bytes[AUDIO_HEADER_BYTES..];
    let want = header.payload_bytes();
    if payload.len() != want {
        return Err(FrameError::PayloadLength {
            got: payload.len(),
            want,
        });
    }
    Ok((header, payload))
}

/// A notification topic a client subscribes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Topic {
    Serial,
    State,
    UiChanged,
    Fault,
    Fidelity,
    Lease,
    Frames,
    /// `AUD1` binary frames and `event.audio_level`.
    Audio,
}

impl Topic {
    pub const ALL: [Topic; 8] = [
        Topic::Serial,
        Topic::State,
        Topic::UiChanged,
        Topic::Fault,
        Topic::Fidelity,
        Topic::Lease,
        Topic::Frames,
        Topic::Audio,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Topic::Serial => "serial",
            Topic::State => "state",
            Topic::UiChanged => "ui_changed",
            Topic::Fault => "fault",
            Topic::Fidelity => "fidelity",
            Topic::Lease => "lease",
            Topic::Frames => "frames",
            Topic::Audio => "audio",
        }
    }

    pub fn parse(name: &str) -> Option<Topic> {
        Topic::ALL.into_iter().find(|t| t.as_str() == name)
    }
}

/// One client's subscription and display backpressure state. A pure state machine, so the rule
/// that an observer never slows or changes virtual time is testable without I/O.
#[derive(Clone, Debug, Default)]
pub struct Session {
    topics: BTreeSet<Topic>,
    pending: Option<(FrameHeader, Rect)>,
    frames_dropped: u32,
}

impl Session {
    pub fn new() -> Session {
        Session::default()
    }

    /// Adds topics; unknown names are returned so the caller can report them.
    pub fn subscribe(&mut self, names: &[&str]) -> Vec<String> {
        let mut unknown = Vec::new();
        for name in names {
            match Topic::parse(name) {
                Some(topic) => {
                    self.topics.insert(topic);
                }
                None => unknown.push((*name).to_string()),
            }
        }
        unknown
    }

    pub fn unsubscribe(&mut self, names: &[&str]) {
        for name in names {
            if let Some(topic) = Topic::parse(name) {
                self.topics.remove(&topic);
            }
        }
    }

    pub fn wants(&self, topic: Topic) -> bool {
        self.topics.contains(&topic)
    }

    pub fn topics(&self) -> Vec<Topic> {
        self.topics.iter().copied().collect()
    }

    /// Whole frames dropped because the client was behind (`frames_dropped` of `event.state`).
    pub fn frames_dropped(&self) -> u32 {
        self.frames_dropped
    }

    /// Offers a frame. The producer never waits: an untaken frame is folded into the pending
    /// rectangle and counted in `frames_dropped`.
    pub fn offer(&mut self, header: FrameHeader) {
        self.pending = Some(match self.pending.take() {
            None => (header, header.rect),
            Some((_, rect)) => {
                self.frames_dropped = self.frames_dropped.saturating_add(1);
                (header, rect.union(header.rect))
            }
        });
    }

    /// Takes the frame to send: the union rectangle with the newest offer's metadata, since the
    /// caller reads the pixels from the current frame memory.
    pub fn take(&mut self) -> Option<FrameHeader> {
        let (header, rect) = self.pending.take()?;
        Some(FrameHeader {
            rect,
            flags: if rect == full_panel() {
                header.flags | FLAG_FULL_FRAME
            } else {
                header.flags
            },
            ..header
        })
    }
}

pub fn full_panel() -> Rect {
    Rect {
        x: 0,
        y: 0,
        w: crate::png::PANEL_WIDTH as u16,
        h: crate::png::PANEL_HEIGHT as u16,
    }
}

pub fn route(router: axum::Router<Arc<Server>>) -> axum::Router<Arc<Server>> {
    router.route("/v1/instances/{id}/ws", axum::routing::any(upgrade))
}

/// The upgrade handler: auth checks, subprotocol, then the pump. The `Origin` check matters most
/// here, because CORS does not govern a WebSocket handshake.
pub async fn upgrade(
    axum::extract::State(server): axum::extract::State<Arc<Server>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    ws: axum::extract::ws::WebSocketUpgrade,
) -> axum::response::Response {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    if let Err(e) =
        server.authorize_headers(header("host"), header("origin"), header("authorization"))
    {
        return crate::http::into_axum(crate::http::Response::unauthorized(&e));
    }
    let instance = match pemu_api::instance::InstanceId::parse(&id) {
        Ok(instance) => instance,
        Err(e) => return crate::http::into_axum(crate::http::Response::error(&e)),
    };
    ws.protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| pump(server, instance, socket))
}

/// Serves one WebSocket until the client goes away. It wakes on a client frame, a producer notice
/// or a finished registry call.
///
/// Registry calls run on the blocking pool one at a time and are not awaited in place, so the
/// stream keeps flowing during a long `run`. When a call leaves the instance without its worker
/// (a `stop`), the pump flushes, sends a final `event.state` of `stopped`, and closes.
async fn pump(
    server: Arc<Server>,
    instance: pemu_api::instance::InstanceId,
    mut socket: axum::extract::ws::WebSocket,
) {
    use axum::extract::ws::Message;

    let hub = server.pool().worker(instance).map(|worker| worker.hub);
    let subscriber = match &hub {
        Some(hub) => hub.subscribe(),
        None => Arc::new(crate::hub::Subscriber::default()),
    };
    let mut queued: std::collections::VecDeque<crate::mcp_stdio::RpcRequest> =
        std::collections::VecDeque::new();
    let mut running: Option<tokio::task::JoinHandle<serde_json::Value>> = None;
    loop {
        if running.is_none()
            && let Some(request) = queued.pop_front()
        {
            // A registry call blocks until the instance thread answers; on an executor worker it
            // would stall every other socket (and, on a current-thread runtime, the health probe
            // and shutdown watcher).
            let server = Arc::clone(&server);
            running = Some(tokio::task::spawn_blocking(move || {
                registry_call(&server, instance, &request)
            }));
        }
        tokio::select! {
            message = socket.recv() => {
                let Some(Ok(message)) = message else { break };
                let reply = match message {
                    Message::Text(text) => {
                        let text = text.to_string();
                        match parse_text(&text) {
                            Err(error) => Some(error),
                            Ok(request) if is_subscription(&request) => {
                                Some(subscription(&subscriber, hub.as_deref(), &request))
                            }
                            Ok(request) => {
                                queued.push_back(request);
                                None
                            }
                        }
                    }
                    Message::Binary(bytes) => {
                        subscriber.with_session(|session| handle_binary(session, &bytes))
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => None,
                };
                if let Some(reply) = reply
                    && socket
                        .send(Message::Text(reply.to_string().into()))
                        .await
                        .is_err()
                {
                    break;
                }
            }
            finished = async { running.as_mut().expect("guarded").await }, if running.is_some() => {
                running = None;
                let reply = finished.unwrap_or_else(|e| {
                    // A panicked task or a runtime going down still gets a JSON-RPC error, not a
                    // reset.
                    rpc_error(
                        serde_json::Value::Null,
                        -32603,
                        &format!("the call did not finish: {e}"),
                    )
                });
                if socket
                    .send(Message::Text(reply.to_string().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                if hub.is_some() && server.pool().worker(instance).is_none() {
                    let latest = hub.as_ref().and_then(|hub| hub.latest());
                    for push in subscriber.drain(latest.as_deref()) {
                        if socket.send(push_message(push)).await.is_err() {
                            return;
                        }
                    }
                    let last = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "event.state",
                        "params": { "state": "stopped" },
                    });
                    let _ = socket.send(Message::Text(last.to_string().into())).await;
                    let _ = socket.send(Message::Close(None)).await;
                    return;
                }
            }
            () = subscriber.notified() => {
                let latest = hub.as_ref().and_then(|hub| hub.latest());
                for push in subscriber.drain(latest.as_deref()) {
                    if socket.send(push_message(push)).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn push_message(push: crate::hub::Push) -> axum::extract::ws::Message {
    use axum::extract::ws::Message;
    match push {
        crate::hub::Push::Text(value) => Message::Text(value.to_string().into()),
        crate::hub::Push::Binary(bytes) => Message::Binary(bytes.into()),
    }
}

fn parse_text(text: &str) -> Result<crate::mcp_stdio::RpcRequest, serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| rpc_error(serde_json::Value::Null, -32700, &e.to_string()))?;
    match cmd_frame_request(&value) {
        Some(request) => Ok(request),
        None => crate::mcp_stdio::RpcRequest::parse(&value).map_err(|why| {
            rpc_error(
                value.get("id").cloned().unwrap_or(serde_json::Value::Null),
                -32600,
                why,
            )
        }),
    }
}

fn is_subscription(request: &crate::mcp_stdio::RpcRequest) -> bool {
    matches!(request.method.as_str(), "subscribe" | "unsubscribe")
}

/// Answers `subscribe` and `unsubscribe` without a thread hop. A new `frames` subscriber is
/// offered the whole panel and a new `state` subscriber the current state, since a quiet screen
/// may never send them otherwise.
fn subscription(
    subscriber: &crate::hub::Subscriber,
    hub: Option<&crate::hub::Hub>,
    request: &crate::mcp_stdio::RpcRequest,
) -> serde_json::Value {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    let names: Vec<String> = request
        .params
        .get("topics")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let (unknown, topics, new_frames, new_state) = subscriber.with_session(|session| {
        let had_frames = session.wants(Topic::Frames);
        let had_state = session.wants(Topic::State);
        let unknown = if request.method == "subscribe" {
            session.subscribe(&refs)
        } else {
            session.unsubscribe(&refs);
            Vec::new()
        };
        let topics: Vec<&'static str> = session.topics().iter().map(|t| t.as_str()).collect();
        (
            unknown,
            topics,
            !had_frames && session.wants(Topic::Frames),
            !had_state && session.wants(Topic::State),
        )
    });
    if let Some(hub) = hub {
        if new_frames {
            hub.offer_full_frame(subscriber);
        }
        if new_state {
            hub.offer_state(subscriber);
        }
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": { "topics": topics, "unknown": unknown },
    })
}

fn registry_call(
    server: &Arc<Server>,
    instance: pemu_api::instance::InstanceId,
    request: &crate::mcp_stdio::RpcRequest,
) -> serde_json::Value {
    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    // Methods arrive in their MCP spelling (`passport_input`), so a client reuses one name table.
    let name = request
        .method
        .strip_prefix(crate::mcp_stdio::TOOL_PREFIX)
        .unwrap_or(&request.method);
    match server.command(name, Some(instance), request.params.clone()) {
        Ok(response) => {
            let body: serde_json::Value =
                serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null);
            if body.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": body.get("result").cloned().unwrap_or(serde_json::Value::Null) })
            } else {
                serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": body.get("error").cloned().unwrap_or(serde_json::Value::Null) })
            }
        }
        Err(e) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "error": e.to_json() }),
    }
}

/// Reads a `{"id":7,"cmd":"run","args":{..}}` frame as a JSON-RPC call of `cmd` with `args` as
/// its parameters; anything else goes to [`crate::mcp_stdio::RpcRequest::parse`]. Both shapes are
/// documented for this socket, so both are accepted. The `id` travels back unchanged, and the
/// answer is always a JSON-RPC object.
fn cmd_frame_request(value: &serde_json::Value) -> Option<crate::mcp_stdio::RpcRequest> {
    if value.get("jsonrpc").is_some() {
        return None;
    }
    let method = value.get("cmd")?.as_str()?.to_string();
    let params = value
        .get("args")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let meta = params
        .get("_meta")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    Some(crate::mcp_stdio::RpcRequest {
        id: value.get("id").cloned(),
        method,
        params,
        meta,
    })
}

fn handle_binary(session: &mut Session, bytes: &[u8]) -> Option<serde_json::Value> {
    let outcome = match decode_audio(bytes) {
        Ok((header, _pcm)) if header.magic == MIC1 => Ok(header),
        Ok((header, _)) => Err(FrameError::WrongDirection {
            magic: header.magic,
        }),
        Err(e) => Err(e),
    };
    match outcome {
        // Accepted and counted, not yet fed to the machine's mic ring.
        Ok(header) => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "event.fidelity",
            "params": {
                "warning": "MIC1 frames are accepted and their headers checked; injecting the PCM \
                            into the guest mic path is not supported yet",
                "n_frames": header.n_frames,
                "sample_rate": header.sample_rate,
                "frames_dropped": session.frames_dropped(),
            },
        })),
        Err(e) => Some(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "event.fault",
            "params": { "error": { "code": "E_USAGE", "message": e.to_string() } },
        })),
    }
}

fn rpc_error(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> FrameHeader {
        FrameHeader {
            frame_no: 42,
            vt_ns: 12_402_000_000,
            rect: Rect {
                x: 10,
                y: 20,
                w: 4,
                h: 3,
            },
            format: PixelFormat::Rgb565Le,
            flags: FLAG_DISPLAY_ON,
            backlight: 1023,
        }
    }

    #[test]
    fn the_frm1_header_is_the_28_byte_layout() {
        let bytes = header().to_bytes();
        assert_eq!(bytes.len(), FRAME_HEADER_BYTES);
        assert_eq!(&bytes[0..4], b"FRM1");
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 42);
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            12_402_000_000
        );
        assert_eq!(u16::from_le_bytes(bytes[16..18].try_into().unwrap()), 10);
        assert_eq!(u16::from_le_bytes(bytes[18..20].try_into().unwrap()), 20);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 4);
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 3);
        assert_eq!(bytes[24], 1, "1 is RGB565LE, the panel's native format");
        assert_eq!(bytes[25], FLAG_DISPLAY_ON);
        assert_eq!(u16::from_le_bytes(bytes[26..28].try_into().unwrap()), 1023);
    }

    #[test]
    fn a_frame_round_trips_with_its_dirty_rectangle() {
        let header = header();
        let payload = vec![0xa5u8; header.payload_bytes()];
        assert_eq!(payload.len(), 4 * 3 * 2);
        let frame = encode_frame(&header, &payload).expect("encode");
        assert_eq!(frame.len(), FRAME_HEADER_BYTES + payload.len());
        let (back, pixels) = decode_frame(&frame).expect("decode");
        assert_eq!(back, header);
        assert_eq!(pixels, payload.as_slice());
    }

    #[test]
    fn a_frame_whose_payload_does_not_match_its_header_is_refused() {
        let header = header();
        assert_eq!(
            encode_frame(&header, &[0; 3]),
            Err(FrameError::PayloadLength { got: 3, want: 24 })
        );
        let mut frame = encode_frame(&header, &[0u8; 24]).expect("encode");
        frame.push(0);
        assert_eq!(
            decode_frame(&frame),
            Err(FrameError::PayloadLength { got: 25, want: 24 })
        );
        assert_eq!(
            decode_frame(&[0; 8]),
            Err(FrameError::TooShort { got: 8, want: 28 })
        );
        let mut wrong = encode_frame(&header, &[0u8; 24]).expect("encode");
        wrong[0..4].copy_from_slice(b"XXXX");
        assert_eq!(
            decode_frame(&wrong),
            Err(FrameError::UnknownMagic(*b"XXXX"))
        );
        let mut format = encode_frame(&header, &[0u8; 24]).expect("encode");
        format[24] = 9;
        assert_eq!(decode_frame(&format), Err(FrameError::UnknownFormat(9)));
    }

    #[test]
    fn an_rgb888_frame_carries_three_bytes_per_pixel() {
        let mut header = header();
        header.format = PixelFormat::Rgb888;
        assert_eq!(header.payload_bytes(), 4 * 3 * 3);
        let frame = encode_frame(&header, &[7u8; 36]).expect("encode");
        let (back, payload) = decode_frame(&frame).expect("decode");
        assert_eq!(back.format, PixelFormat::Rgb888);
        assert_eq!(payload.len(), 36);
    }

    #[test]
    fn the_audio_header_is_the_24_byte_layout_and_both_magics_travel() {
        for (magic, dir) in [(AUD1, 0u8), (MIC1, 1u8)] {
            let header = AudioHeader {
                magic,
                vt_ns: 7,
                sample_rate: 16_000,
                channels: 1,
                bits: 16,
                dir,
                n_frames: 5,
            };
            let bytes = header.to_bytes();
            assert_eq!(bytes.len(), AUDIO_HEADER_BYTES);
            assert_eq!(&bytes[0..4], &magic);
            assert_eq!(u64::from_le_bytes(bytes[4..12].try_into().unwrap()), 7);
            assert_eq!(
                u32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                16_000
            );
            assert_eq!(bytes[16], 1);
            assert_eq!(bytes[17], 16);
            assert_eq!(bytes[18], dir);
            assert_eq!(bytes[19], 0, "byte 19 is reserved and stays zero");
            assert_eq!(u32::from_le_bytes(bytes[20..24].try_into().unwrap()), 5);

            let pcm = vec![0u8; header.payload_bytes()];
            assert_eq!(pcm.len(), 10);
            let frame = encode_audio(&header, &pcm).expect("encode");
            let (back, payload) = decode_audio(&frame).expect("decode");
            assert_eq!(back, header);
            assert_eq!(payload.len(), 10);
        }
    }

    #[test]
    fn a_binary_frame_of_an_unknown_kind_is_refused() {
        let mut frame = vec![0u8; AUDIO_HEADER_BYTES];
        frame[0..4].copy_from_slice(b"WAV1");
        assert_eq!(
            decode_audio(&frame),
            Err(FrameError::UnknownMagic(*b"WAV1"))
        );
        assert_eq!(
            decode_audio(&[0; 4]),
            Err(FrameError::TooShort { got: 4, want: 24 })
        );
    }

    #[test]
    fn subscribing_names_the_known_topics_and_reports_the_rest() {
        let mut session = Session::new();
        let unknown = session.subscribe(&["serial", "frames", "telepathy"]);
        assert_eq!(unknown, ["telepathy"]);
        assert!(session.wants(Topic::Serial));
        assert!(session.wants(Topic::Frames));
        assert!(!session.wants(Topic::Audio));
        session.unsubscribe(&["serial"]);
        assert!(!session.wants(Topic::Serial));
        assert_eq!(session.topics(), [Topic::Frames]);
        for topic in Topic::ALL {
            assert_eq!(Topic::parse(topic.as_str()), Some(topic));
        }
    }

    #[test]
    fn a_slow_client_gets_the_union_of_what_it_missed_and_a_dropped_count() {
        let mut session = Session::new();
        let frame = |no: u32, x: u16, y: u16, w: u16, h: u16| FrameHeader {
            frame_no: no,
            vt_ns: u64::from(no) * 1_000_000,
            rect: Rect { x, y, w, h },
            ..header()
        };
        session.offer(frame(1, 0, 0, 10, 10));
        session.offer(frame(2, 100, 200, 10, 10));
        session.offer(frame(3, 50, 50, 10, 10));
        assert_eq!(
            session.frames_dropped(),
            2,
            "the producer never waits for an observer"
        );
        let taken = session.take().expect("a pending frame");
        assert_eq!(
            taken.rect,
            Rect {
                x: 0,
                y: 0,
                w: 110,
                h: 210
            },
            "the client receives the union of the rectangles it missed"
        );
        assert_eq!(
            taken.frame_no, 3,
            "the metadata is the newest frame's, because the pixels are read from now"
        );
        assert_eq!(session.take(), None, "taking twice yields nothing");
    }

    #[test]
    fn a_union_that_covers_the_panel_is_marked_a_full_frame() {
        let mut session = Session::new();
        session.offer(FrameHeader {
            rect: Rect {
                x: 0,
                y: 0,
                w: 240,
                h: 160,
            },
            flags: FLAG_DISPLAY_ON,
            ..header()
        });
        session.offer(FrameHeader {
            rect: Rect {
                x: 0,
                y: 160,
                w: 240,
                h: 160,
            },
            flags: FLAG_DISPLAY_ON,
            ..header()
        });
        let taken = session.take().expect("a pending frame");
        assert_eq!(taken.rect, full_panel());
        assert_eq!(taken.flags & FLAG_FULL_FRAME, FLAG_FULL_FRAME);
        assert_eq!(taken.flags & FLAG_DISPLAY_ON, FLAG_DISPLAY_ON);
    }

    /// With the board default `invon_shows_ram = true`, an INVON frame of the official sky
    /// (0x145D) arrives with the complement bit clear.
    #[test]
    fn the_powered_bit_follows_the_rail_alone() {
        let mut port = pemu_core::hostio::FramePort::new();
        port.set_display_on(true);
        port.set_powered(false);
        let off = frame_flags(&port);
        assert_eq!(off & FLAG_POWERED, 0, "rail down");
        assert_ne!(
            off & FLAG_DISPLAY_ON,
            0,
            "DISPON is the panel's state, kept apart"
        );
        port.set_powered(true);
        let on = frame_flags(&port);
        assert_eq!(on & FLAG_POWERED, FLAG_POWERED);
        assert_eq!(on & !FLAG_POWERED, off, "no other bit moves with the rail");
        let bits = [
            FLAG_FULL_FRAME,
            FLAG_DISPLAY_ON,
            FLAG_INVERTED,
            FLAG_SLEEP_IN,
            FLAG_GLASS_COMPLEMENT,
            FLAG_POWERED,
        ];
        assert_eq!(
            bits.iter().fold(0u8, |all, bit| {
                assert_eq!(all & bit, 0, "bit {bit:#x} is assigned twice");
                all | bit
            }),
            0b0011_1111
        );
    }

    #[test]
    fn an_invon_frame_of_the_sky_carries_no_glass_complement_under_invon_shows_ram() {
        let mut port = pemu_core::hostio::FramePort::new();
        port.set_powered(true);
        port.set_sleeping(false);
        port.set_display_on(true);
        port.set_inverted(true);
        assert!(port.invon_shows_ram(), "the board default shows RAM");
        port.pixels_mut().fill(0x145D);

        let rect = Rect {
            x: 3,
            y: 5,
            w: 2,
            h: 1,
        };
        let header = FrameHeader {
            frame_no: 1,
            vt_ns: 0,
            rect,
            format: PixelFormat::Rgb565Le,
            flags: frame_flags(&port),
            backlight: 1023,
        };
        let bytes =
            encode_frame(&header, &frame_payload(port.pixels(), rect)).expect("a valid frame");
        let (decoded, payload) = decode_frame(&bytes).expect("round trip");
        assert_ne!(decoded.flags & FLAG_INVERTED, 0, "INVON is still reported");
        assert_ne!(decoded.flags & FLAG_POWERED, 0, "the rail is on");
        assert_ne!(decoded.flags & FLAG_DISPLAY_ON, 0);
        assert_eq!(decoded.flags & FLAG_SLEEP_IN, 0);
        assert_eq!(
            decoded.flags & FLAG_GLASS_COMPLEMENT,
            0,
            "INVON under invon_shows_ram shows memory: complement=false"
        );
        assert_eq!(
            payload,
            [0x5D, 0x14, 0x5D, 0x14],
            "the payload is panel memory, LE"
        );

        // The rule, not the INVON bit, decides: INVOFF complements, and a board with the rule off
        // exchanges the two.
        port.set_inverted(false);
        assert_ne!(frame_flags(&port) & FLAG_GLASS_COMPLEMENT, 0);
        port.set_invon_shows_ram(false);
        assert_eq!(frame_flags(&port) & FLAG_GLASS_COMPLEMENT, 0);
        port.set_inverted(true);
        assert_ne!(frame_flags(&port) & FLAG_GLASS_COMPLEMENT, 0);
        port.set_display_on(false);
        assert_eq!(frame_flags(&port) & FLAG_DISPLAY_ON, 0);
    }

    #[test]
    fn a_client_that_keeps_up_drops_nothing() {
        let mut session = Session::new();
        for no in 0..10u32 {
            session.offer(FrameHeader {
                frame_no: no,
                ..header()
            });
            assert!(session.take().is_some());
        }
        assert_eq!(session.frames_dropped(), 0);
    }

    #[test]
    fn the_rectangle_union_is_the_smallest_covering_box() {
        let a = Rect {
            x: 4,
            y: 4,
            w: 2,
            h: 2,
        };
        let b = Rect {
            x: 0,
            y: 8,
            w: 2,
            h: 2,
        };
        assert_eq!(
            a.union(b),
            Rect {
                x: 0,
                y: 4,
                w: 6,
                h: 6
            }
        );
        assert_eq!(a.union(a), a);
        assert_eq!(a.pixels(), 4);
    }
    // The protocol over a real socket, with `tungstenite` as the client.

    use crate::auth::{Auth, Token};
    use crate::daemon::Shutdown;
    use crate::pool::Pool;
    use pemu_api::error::ApiError;
    use pemu_api::output::Output;
    use pemu_api::receipt::Receipt;
    use pemu_api::spec::{
        Annotations, CapsGroup, CliShape, CommandSpec, Example, HandlerCx, any_schema,
    };
    use std::collections::BTreeSet;

    fn echo(_cx: &mut HandlerCx, args: serde_json::Value) -> Result<Output, ApiError> {
        Ok(Output::new(
            serde_json::json!({ "echo": args }),
            "echoed",
            Receipt::default(),
        ))
    }

    static SPECS: &[CommandSpec] = &[CommandSpec {
        name: "input",
        group: CapsGroup::Core,
        summary: "A command the WebSocket test calls.",
        input_schema: any_schema,
        output_schema: any_schema,
        annotations: Annotations {
            needs_instance: true,
            ..Annotations::EMPTY
        },
        cli: CliShape::EMPTY,
        scenario_step: None,
        examples: &[Example {
            title: "input",
            args: "{}",
        }],
        errors: &[],
        handler: echo,
    }];

    fn token() -> Token {
        Token::from_bytes([0x6c; 32])
    }

    /// Serves on a kernel-chosen port, so a test never fights a developer's running daemon.
    fn serve_ws() -> (u16, Arc<Server>, std::thread::JoinHandle<()>) {
        let bound = crate::daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(
            Server::new(
                Auth::new(token(), port),
                Arc::new(Pool::new(2)),
                Shutdown::new(),
                BTreeSet::from([CapsGroup::Core]),
            )
            .with_commands(SPECS),
        );
        server.pool().host_for_test("p901");
        let listener = bound.listener;
        let served = Arc::clone(&server);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a tokio runtime");
            runtime
                .block_on(crate::http::serve(listener, served))
                .expect("serve");
        });
        (port, server, thread)
    }

    fn client_request(port: u16, path: &str) -> tungstenite::handshake::client::Request {
        use tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}{path}")
            .into_client_request()
            .expect("a client request");
        let headers = request.headers_mut();
        headers.insert(
            "authorization",
            format!("Bearer {}", token().to_hex())
                .parse()
                .expect("a header value"),
        );
        headers.insert(
            "origin",
            format!("http://127.0.0.1:{port}")
                .parse()
                .expect("a header value"),
        );
        headers.insert(
            "sec-websocket-protocol",
            SUBPROTOCOL.parse().expect("a header value"),
        );
        request
    }

    fn read_json(
        socket: &mut tungstenite::WebSocket<impl std::io::Read + std::io::Write>,
    ) -> serde_json::Value {
        match socket.read().expect("a message") {
            tungstenite::Message::Text(text) => serde_json::from_str(text.as_str()).expect("JSON"),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    #[test]
    fn the_socket_speaks_the_subprotocol_json_rpc_and_the_mic1_frame() {
        let (port, server, thread) = serve_ws();
        let (mut socket, response) =
            tungstenite::connect(client_request(port, "/v1/instances/p901/ws")).expect("connect");
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some(SUBPROTOCOL),
            "the server agrees to the subprotocol"
        );

        socket
            .send(tungstenite::Message::text(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "passport_input",
                    "params": { "button": "down", "action": "click" }
                })
                .to_string(),
            ))
            .expect("send");
        let answer = read_json(&mut socket);
        assert_eq!(answer["id"], 7);
        assert_eq!(answer["result"]["echo"]["button"], "down");

        // The `cmd` shape, with no `jsonrpc` member, reaches the same command and keeps its `id`.
        socket
            .send(tungstenite::Message::text(
                serde_json::json!({
                    "id": 71,
                    "cmd": "input",
                    "args": { "button": "up", "action": "click" }
                })
                .to_string(),
            ))
            .expect("send");
        let answer = read_json(&mut socket);
        assert_eq!(answer["id"], 71);
        assert_eq!(
            answer["result"]["echo"]["button"], "up",
            "a client that followed the protocol to the letter must not be refused"
        );

        socket
            .send(tungstenite::Message::text(
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 8,
                    "method": "subscribe",
                    "params": { "topics": ["serial", "frames", "telepathy"] }
                })
                .to_string(),
            ))
            .expect("send");
        let answer = read_json(&mut socket);
        assert_eq!(
            answer["result"]["topics"],
            serde_json::json!(["serial", "frames"]),
            "topics come back in wire order, never a hash order"
        );
        assert_eq!(
            answer["result"]["unknown"],
            serde_json::json!(["telepathy"])
        );

        let header = AudioHeader {
            magic: MIC1,
            vt_ns: 1_000,
            sample_rate: 16_000,
            channels: 1,
            bits: 16,
            dir: 1,
            n_frames: 4,
        };
        let frame = encode_audio(&header, &[0u8; 8]).expect("encode");
        socket
            .send(tungstenite::Message::binary(frame))
            .expect("send");
        let answer = read_json(&mut socket);
        assert_eq!(answer["method"], "event.fidelity");
        assert_eq!(answer["params"]["n_frames"], 4);

        // An `AUD1` frame travels the other way and is refused as a fault, not a disconnect.
        let wrong = encode_audio(
            &AudioHeader {
                magic: AUD1,
                ..header
            },
            &[0u8; 8],
        )
        .expect("encode");
        socket
            .send(tungstenite::Message::binary(wrong))
            .expect("send");
        let answer = read_json(&mut socket);
        assert_eq!(answer["method"], "event.fault");
        assert!(
            answer["params"]["error"]["message"]
                .as_str()
                .expect("a message")
                .contains("AUD1")
        );

        socket.close(None).expect("close");
        server.shutdown_now(pemu_core::time::VTime::default());
        thread.join().expect("the server thread ends");
    }

    #[test]
    fn a_cross_site_page_cannot_open_the_socket() {
        let (port, server, thread) = serve_ws();
        // CORS does not govern a WebSocket handshake, so `Origin` is the only defense.
        let mut request = client_request(port, "/v1/instances/p901/ws");
        request.headers_mut().insert(
            "origin",
            "http://evil.example".parse().expect("a header value"),
        );
        let refused = tungstenite::connect(request).expect_err("the handshake must fail");
        match refused {
            tungstenite::Error::Http(response) => assert_eq!(response.status(), 403),
            other => panic!("expected an HTTP refusal, got {other:?}"),
        }

        let mut request = client_request(port, "/v1/instances/p901/ws");
        request.headers_mut().remove("authorization");
        let refused = tungstenite::connect(request).expect_err("the handshake must fail");
        match refused {
            tungstenite::Error::Http(response) => assert_eq!(response.status(), 401),
            other => panic!("expected an HTTP refusal, got {other:?}"),
        }

        // A foreign `Host` is the DNS-rebinding shape: the connection reaches 127.0.0.1 and only
        // the header names the attacker.
        let mut request = client_request(port, "/v1/instances/p901/ws");
        request
            .headers_mut()
            .insert("host", "evil.example:8080".parse().expect("a header value"));
        let refused = tungstenite::connect(request).expect_err("the handshake must fail");
        match refused {
            tungstenite::Error::Http(response) => assert_eq!(response.status(), 403),
            other => panic!("expected an HTTP refusal, got {other:?}"),
        }

        server.shutdown_now(pemu_core::time::VTime::default());
        thread.join().expect("the server thread ends");
    }
}
