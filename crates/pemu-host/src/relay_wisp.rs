//! The server side of the Wi-Fi bridge: a WISP v1 server (the MercuryWorkshop `wisp-protocol`
//! specification) that opens host sockets for the virtual LAN streams (`pemu_radio::lan::bridge`).
//!
//! Two carriers share it. Native bridge mode (`net_http --op bridge`) runs the server in process:
//! at every slice boundary [`tick`] moves the machine's packets through it, journals the answers
//! as `InputEvent::NetFrame` and paces the run at `Wall { rate: 1 }`. The browser relay
//! (`/v1/relay`) runs one server per WebSocket ([`serve_relay`]) under the same auth as every other
//! route, since an unauthenticated loopback relay is an open proxy.
//!
//! Policy: a `CONNECT` is accepted only for TCP to [`HOST_NAME`] on an allowlisted port, and
//! reaches `127.0.0.1` on the route's host port. Everything else (public, private or link-local
//! destinations, UDP, and any port this process listens on, see [`deny_port`]) is closed with
//! `0x48`. More than [`MAX_STREAMS`] open streams is `0x49`.
//!
//! Flow control: WISP v1 credit counts packets. The server announces [`BUFFER_PACKETS`] on stream
//! 0, sends a stream's `CONTINUE` once its socket connects (which lets the virtual LAN complete
//! the guest's handshake), and renews it after writing what the client sent. A socket whose send
//! buffer is full holds the renewal back.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use net_http::wisp::{self as bridge, HOST_NAME, kind, reason};
use pemu_api::commands::net_http::{self, NetBridgeIo};
use pemu_api::commands::start::Session;
use pemu_api::instance::InstanceId;
use pemu_core::input::NetRoute;

use crate::pacing::Pacer;

/// The per-stream credit the server announces, in packets.
pub const BUFFER_PACKETS: u32 = 64;
/// The most streams one server holds open.
pub const MAX_STREAMS: usize = 16;
/// How long a loopback connect may take. It only bounds a wedged host: on Windows that needs SYN
/// retransmission turned off, which `platform::connect_loopback` does.
pub const CONNECT_WAIT: Duration = Duration::from_millis(500);
/// The most bytes one socket read takes, which is also the largest `DATA` payload sent.
pub const READ_CHUNK: usize = 16 * 1024;
/// The most bytes a socket may have waiting before the server stops renewing the stream's credit.
pub const MAX_PENDING_WRITE: usize = 256 * 1024;

// Ports that are never a destination.

/// The loopback ports this process listens on. Process-wide on purpose: a port belongs to the OS,
/// so no bridge of any pool may reach a listener any pool opened.
fn denied() -> &'static Mutex<BTreeSet<u16>> {
    static DENIED: OnceLock<Mutex<BTreeSet<u16>>> = OnceLock::new();
    DENIED.get_or_init(Mutex::default)
}

/// Records a port this process listens on, which no bridge may reach. Every loopback listener this
/// crate binds calls it.
pub fn deny_port(port: u16) {
    denied()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(port);
}

pub fn is_denied(port: u16) -> bool {
    denied()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&port)
}

pub fn check_routes(routes: &[NetRoute]) -> Result<(), String> {
    match routes.iter().find(|r| is_denied(r.host_port)) {
        Some(r) => Err(format!(
            "host port {} is one this process listens on (the daemon or a USB endpoint), which a \
             bridge never reaches",
            r.host_port
        )),
        None => Ok(()),
    }
}

// The server.

#[derive(Debug)]
struct Stream {
    socket: TcpStream,
    pending: Vec<u8>,
    used: u32,
}

/// A WISP v1 server over host sockets. Plain data plus sockets, no thread: the caller feeds it
/// client packets, polls it and takes its answers.
#[derive(Debug)]
pub struct WispServer {
    routes: Vec<NetRoute>,
    streams: BTreeMap<u32, Stream>,
    out: VecDeque<Vec<u8>>,
}

impl WispServer {
    /// A server for these routes. Its first packet is the stream-0 `CONTINUE`.
    pub fn new(routes: &[NetRoute]) -> WispServer {
        let mut out = VecDeque::new();
        out.push_back(bridge::continue_(0, BUFFER_PACKETS));
        WispServer {
            routes: routes.to_vec(),
            streams: BTreeMap::new(),
            out,
        }
    }

    pub fn routes(&self) -> &[NetRoute] {
        &self.routes
    }

    pub fn open_streams(&self) -> usize {
        self.streams.len()
    }

    /// Where a `CONNECT` to `host:port` goes, or the `CLOSE` reason that refuses it.
    pub fn resolve(&self, stream_type: u8, host: &str, port: u16) -> Result<SocketAddr, u8> {
        if stream_type != bridge::STREAM_TCP {
            return Err(reason::BLOCKED);
        }
        if !host.eq_ignore_ascii_case(HOST_NAME) {
            return Err(reason::BLOCKED);
        }
        let route = self
            .routes
            .iter()
            .find(|r| r.port == port)
            .ok_or(reason::BLOCKED)?;
        if is_denied(route.host_port) {
            return Err(reason::BLOCKED);
        }
        Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, route.host_port)))
    }

    pub fn client_packet(&mut self, packet: &[u8]) {
        let Some((ty, id, payload)) = bridge::decode(packet) else {
            return;
        };
        match ty {
            kind::CONNECT => self.connect(id, payload),
            kind::DATA => {
                if let Some(stream) = self.streams.get_mut(&id) {
                    stream.pending.extend_from_slice(payload);
                    stream.used += 1;
                }
            }
            kind::CLOSE => {
                if let Some(stream) = self.streams.remove(&id) {
                    let _ = stream.socket.shutdown(Shutdown::Both);
                }
            }
            _ => {}
        }
    }

    fn connect(&mut self, id: u32, payload: &[u8]) {
        if id == 0 || self.streams.contains_key(&id) {
            self.out.push_back(bridge::close(id, reason::INVALID));
            return;
        }
        let Some((ty, port, host)) = bridge::parse_connect(payload) else {
            self.out.push_back(bridge::close(id, reason::INVALID));
            return;
        };
        let addr = match self.resolve(ty, &host, port) {
            Ok(addr) => addr,
            Err(why) => {
                self.out.push_back(bridge::close(id, why));
                return;
            }
        };
        if self.streams.len() >= MAX_STREAMS {
            self.out.push_back(bridge::close(id, reason::THROTTLED));
            return;
        }
        let socket = match crate::platform::connect_loopback(&addr, CONNECT_WAIT) {
            Ok(socket) => socket,
            Err(err) => {
                let why = match err.kind() {
                    io::ErrorKind::ConnectionRefused => reason::REFUSED,
                    io::ErrorKind::TimedOut => 0x43,
                    _ => reason::NETWORK,
                };
                self.out.push_back(bridge::close(id, why));
                return;
            }
        };
        if socket.set_nonblocking(true).is_err() {
            self.out.push_back(bridge::close(id, reason::NETWORK));
            return;
        }
        let _ = socket.set_nodelay(true);
        self.streams.insert(
            id,
            Stream {
                socket,
                pending: Vec::new(),
                used: 0,
            },
        );
        // The stream's own credit, which is also the "connected" the router waits for.
        self.out.push_back(bridge::continue_(id, BUFFER_PACKETS));
    }

    pub fn poll(&mut self) {
        let mut closed: Vec<(u32, u8)> = Vec::new();
        let mut buf = vec![0u8; READ_CHUNK];
        for (&id, stream) in &mut self.streams {
            // Write first, so a request goes out before its answer is looked for.
            while !stream.pending.is_empty() {
                match stream.socket.write(&stream.pending) {
                    Ok(0) => break,
                    Ok(n) => {
                        stream.pending.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        closed.push((id, reason::NETWORK));
                        break;
                    }
                }
            }
            if stream.used > 0 && stream.pending.len() < MAX_PENDING_WRITE {
                self.out.push_back(bridge::continue_(id, BUFFER_PACKETS));
                stream.used = 0;
            }
            loop {
                match stream.socket.read(&mut buf) {
                    Ok(0) => {
                        closed.push((id, reason::VOLUNTARY));
                        break;
                    }
                    Ok(n) => self
                        .out
                        .push_back(bridge::encode(kind::DATA, id, &buf[..n])),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => {
                        closed.push((id, reason::NETWORK));
                        break;
                    }
                }
            }
        }
        for (id, why) in closed {
            if let Some(stream) = self.streams.remove(&id) {
                let _ = stream.socket.shutdown(Shutdown::Both);
                self.out.push_back(bridge::close(id, why));
            }
        }
    }

    pub fn take(&mut self) -> Vec<Vec<u8>> {
        self.out.drain(..).collect()
    }

    pub fn close_all(&mut self) {
        for (_, stream) in std::mem::take(&mut self.streams) {
            let _ = stream.socket.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for WispServer {
    fn drop(&mut self) {
        self.close_all();
    }
}

// Native bridge mode.

/// One instance's native bridge: its server, the cursor into the machine's outbound window, the
/// next inbound `seq`, and the run's pacer.
#[derive(Debug)]
struct Native {
    server: WispServer,
    cursor: u64,
    seq: Option<u64>,
    pacer: Option<Pacer>,
}

/// The native bridges of one pool's instances, a table of that pool: an `InstanceId` is only
/// unique within its pool, so a process-wide table would let another pool's `p1` take over this
/// one's bridge. Dropping the pool closes its servers' sockets.
#[derive(Debug, Default)]
struct Natives(BTreeMap<InstanceId, Native>);

/// The pacers of instances whose live bridge has no native server here (the BLE HCI bridge), so
/// their runs are paced too. A table of the pool, like [`Natives`].
#[derive(Debug, Default)]
struct Pacers(BTreeMap<InstanceId, Pacer>);

/// Installs the native bridge as `net_http`'s transport, and [`tick`] as what `run` calls at a
/// slice boundary while a bridge is live.
pub fn install() {
    net_http::set_bridge_io(NetBridgeIo { attach, detach });
    net_http::set_bridge_tick(tick);
}

fn attach(session: &Session, routes: &[NetRoute]) -> Result<(), String> {
    let id = session.id;
    check_routes(routes)?;
    // A re-attach starts a fresh server: its stream-0 `CONTINUE` tells the machine the relay is
    // ready again.
    let replaced = session.with_table(|natives: &mut Natives| {
        natives.0.insert(
            id,
            Native {
                server: WispServer::new(routes),
                cursor: u64::MAX,
                seq: None,
                pacer: None,
            },
        )
    });
    // Its sockets close outside the table's lock.
    drop(replaced);
    Ok(())
}

fn detach(session: &Session) {
    let id = session.id;
    let removed = session.with_table(|natives: &mut Natives| natives.0.remove(&id));
    // Its sockets close outside the table's lock.
    drop(removed);
}

pub fn native_streams(session: &Session) -> Option<usize> {
    let id = session.id;
    session.with_table(|natives: &mut Natives| natives.0.get(&id).map(|n| n.server.open_streams()))
}

fn vt(session: &Session) -> Duration {
    Duration::from_micros(session.now().as_us())
}

/// A slice boundary of a run with a live bridge: moves the native bridge's packets both ways and
/// holds the run at `Wall { rate: 1 }`.
pub fn tick(session: &mut Session, first: bool) {
    let id = session.id;
    let now_vt = vt(session);
    let tables = session.tables();
    let pacer = tables.with(|natives: &mut Natives| {
        let native = natives.0.get_mut(&id)?;
        Some(native_turn(session, native, first, now_vt))
    });
    let Some(mut pacer) = pacer else {
        // No native server (the BLE bridge, or a Wi-Fi bridge restored without a transport): the
        // run is still paced.
        let mut pacer = tables.with(|pacers: &mut Pacers| {
            let pacer = pacers
                .0
                .entry(id)
                .or_insert_with(|| Pacer::new(1.0, Instant::now(), now_vt));
            if first {
                pacer.reanchor(Instant::now(), now_vt);
            }
            *pacer
        });
        pacer.wait(now_vt);
        tables.with(|pacers: &mut Pacers| pacers.0.insert(id, pacer));
        return;
    };
    // Sleep with the table's lock released, so other instances' runs are not held up.
    pacer.wait(now_vt);
    tables.with(|natives: &mut Natives| {
        if let Some(native) = natives.0.get_mut(&id) {
            native.pacer = Some(pacer);
        }
    });
}

fn native_turn(session: &mut Session, native: &mut Native, first: bool, now_vt: Duration) -> Pacer {
    if native.cursor == u64::MAX {
        // First look after an attach: older packets in the window belong to no server.
        native.cursor = net_http::bridge_outbound(session, 0)
            .map(|(_, next, _)| next)
            .unwrap_or(0);
    }
    if let Ok((packets, next, _dropped)) = net_http::bridge_outbound(session, native.cursor) {
        native.cursor = next;
        for packet in &packets {
            native.server.client_packet(packet);
        }
    }
    native.server.poll();
    let seq = native
        .seq
        .get_or_insert_with(|| net_http::bridge_next_seq(session));
    for packet in native.server.take() {
        if net_http::bridge_inbound(session, *seq, packet).is_ok() {
            *seq += 1;
        }
    }
    let pacer = native
        .pacer
        .get_or_insert_with(|| Pacer::new(1.0, Instant::now(), now_vt));
    if first {
        pacer.reanchor(Instant::now(), now_vt);
    }
    *pacer
}

// The browser carrier: `/v1/relay`.

pub const RELAY_PATH: &str = "/v1/relay";

/// The `hello` value of a relay page's first frame (`attach::HELLO` is the other socket's).
pub const RELAY_HELLO: &str = "passportsim-relay";

pub const RELAY_HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a relay socket lets its server read its host sockets: the analogue of the native
/// carrier's slice boundary, bounding how long an answer waits on the host side.
pub const RELAY_POLL: Duration = Duration::from_millis(5);

/// The routes of a `hello` frame, checked again at this end (route count, no port 0, no guest port
/// twice, no port this process listens on), so a bad port is refused at the hello, where the page
/// can report it, rather than at the first `CONNECT`.
fn hello_routes(text: &str) -> Result<Vec<NetRoute>, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|_| "the first frame is one JSON object".to_string())?;
    if value.get("hello").and_then(serde_json::Value::as_str) != Some(RELAY_HELLO) {
        return Err(format!(
            "the first frame must be {{\"hello\": \"{RELAY_HELLO}\", \"routes\": [..]}}"
        ));
    }
    let list = value
        .get("routes")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "`routes` is an array of {port, host_port}".to_string())?;
    let mut routes = Vec::with_capacity(list.len());
    for route in list {
        let port = |name: &str| {
            route
                .get(name)
                .and_then(serde_json::Value::as_u64)
                .and_then(|p| u16::try_from(p).ok())
                .filter(|p| *p != 0)
        };
        let guest = port("port").ok_or_else(|| "a route needs `port`, a TCP port".to_string())?;
        routes.push(NetRoute {
            port: guest,
            host_port: port("host_port").unwrap_or(guest),
        });
    }
    if !NetRoute::check_all(&routes) {
        return Err(format!(
            "at most {} routes, each guest port once",
            pemu_core::input::NET_MAX_ROUTES
        ));
    }
    check_routes(&routes)?;
    Ok(routes)
}

pub fn route(
    router: axum::Router<std::sync::Arc<crate::http::Server>>,
) -> axum::Router<std::sync::Arc<crate::http::Server>> {
    router.route(RELAY_PATH, axum::routing::any(upgrade))
}

/// The upgrade: the same `Server::authorize` as every route, then [`serve_relay`]. A browser
/// cannot set `Authorization` on a WebSocket, so a page arrives with its session cookie and a
/// native client with the bearer header; no credential travels inside the socket.
async fn upgrade(
    axum::extract::State(server): axum::extract::State<std::sync::Arc<crate::http::Server>>,
    headers: axum::http::HeaderMap,
    ws: Result<
        axum::extract::ws::WebSocketUpgrade,
        axum::extract::ws::rejection::WebSocketUpgradeRejection,
    >,
) -> axum::response::Response {
    let mut request = crate::http::Request::new(crate::http::Method::Get, RELAY_PATH);
    for (name, value) in &headers {
        if let Ok(value) = value.to_str() {
            request = request.header(name.as_str(), value);
        }
    }
    if let Err(e) = server.authorize(&request) {
        return crate::http::into_axum(crate::http::Response::unauthorized(&e));
    }
    match ws {
        Ok(ws) => ws.on_upgrade(serve_relay),
        Err(_) => crate::http::into_axum(crate::http::Response::error(
            &pemu_api::error::ApiError::new(
                pemu_api::error::E_USAGE,
                "`/v1/relay` is a WebSocket upgrade; a page carries the bridge with \
                 `new WebSocket(..)`",
            ),
        )),
    }
}

/// Serves one page's bridge until it or the daemon closes the socket.
///
/// | Direction | Frame | Meaning |
/// |---|---|---|
/// | page to daemon | text `{"hello": "passportsim-relay", "routes": [{"port": 80, "host_port": 18080}]}` | the first frame; anything else closes the socket |
/// | daemon to page | text `{"ready": true, "routes": [..], "buffer": 64}` | the allowlist this server applied |
/// | daemon to page | text `{"error": ".."}` then a close | the hello was refused |
/// | page to daemon | binary | one WISP v1 client packet, as the machine's window holds it |
/// | daemon to page | binary | one WISP v1 server packet, to be journaled as `InputEvent::NetFrame` |
///
/// One packet per binary frame, since a WebSocket frame already carries its length.
pub async fn serve_relay(mut socket: axum::extract::ws::WebSocket) {
    use axum::extract::ws::Message;

    let hello = match tokio::time::timeout(RELAY_HELLO_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => hello_routes(text.as_str()),
        _ => Err(format!(
            "the first frame must be {{\"hello\": \"{RELAY_HELLO}\", \"routes\": [..]}}"
        )),
    };
    let routes = match hello {
        Ok(routes) => routes,
        Err(why) => {
            let refusal = serde_json::json!({ "error": why });
            let _ = socket.send(Message::Text(refusal.to_string().into())).await;
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let ready = serde_json::json!({
        "ready": true,
        "buffer": BUFFER_PACKETS,
        "routes": routes
            .iter()
            .map(|r| serde_json::json!({ "port": r.port, "host_port": r.host_port }))
            .collect::<Vec<_>>(),
    });
    if socket
        .send(Message::Text(ready.to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    let mut server = WispServer::new(&routes);
    let mut ticker = tokio::time::interval(RELAY_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            message = socket.recv() => match message {
                Some(Ok(Message::Binary(bytes))) => server.client_packet(&bytes),
                Some(Ok(Message::Text(_) | Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
            },
            _ = ticker.tick() => {
                server.poll();
                for packet in server.take() {
                    if socket.send(Message::Binary(packet.into())).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
    server.close_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn route(port: u16, host_port: u16) -> NetRoute {
        NetRoute { port, host_port }
    }

    fn decoded(packets: Vec<Vec<u8>>) -> Vec<(u8, u32, Vec<u8>)> {
        packets
            .iter()
            .map(|p| {
                let (k, s, b) = bridge::decode(p).unwrap();
                (k, s, b.to_vec())
            })
            .collect()
    }

    fn poll_for(server: &mut WispServer) -> Vec<(u8, u32, Vec<u8>)> {
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            server.poll();
            let got = server.take();
            if !got.is_empty() || Instant::now() > until {
                return decoded(got);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn the_server_opens_with_the_stream_0_continue() {
        let mut s = WispServer::new(&[]);
        assert_eq!(
            decoded(s.take()),
            vec![(kind::CONTINUE, 0, BUFFER_PACKETS.to_le_bytes().to_vec())]
        );
    }

    #[test]
    fn the_policy_refuses_everything_but_allowlisted_host_emu_internal_ports() {
        let s = WispServer::new(&[route(80, 18_080), route(81, 18_081)]);
        assert_eq!(
            s.resolve(bridge::STREAM_TCP, "host.emu.internal", 80),
            Ok(SocketAddr::from((Ipv4Addr::LOCALHOST, 18_080)))
        );
        assert!(
            s.resolve(bridge::STREAM_TCP, "HOST.EMU.INTERNAL", 80)
                .is_ok(),
            "the host name is matched without regard to case"
        );
        for (ty, host, port) in [
            (bridge::STREAM_TCP, "example.com", 80),
            (bridge::STREAM_TCP, "127.0.0.1", 80),
            (bridge::STREAM_TCP, "169.254.169.254", 80),
            (bridge::STREAM_TCP, "host.emu.internal", 8_080),
            (0x02, "host.emu.internal", 80),
        ] {
            assert_eq!(
                s.resolve(ty, host, port),
                Err(reason::BLOCKED),
                "{host}:{port}"
            );
        }
        deny_port(18_081);
        assert_eq!(
            s.resolve(bridge::STREAM_TCP, "host.emu.internal", 81),
            Err(reason::BLOCKED)
        );
        assert!(check_routes(&[route(81, 18_081)]).is_err());
        assert!(check_routes(&[route(80, 18_080)]).is_ok());
    }

    #[test]
    fn a_stream_carries_both_directions_over_a_loopback_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            sock.write_all(b"world").unwrap();
        });
        let mut s = WispServer::new(&[route(80, port)]);
        s.take();
        s.client_packet(&bridge::connect(1, 80, HOST_NAME));
        assert_eq!(
            decoded(s.take()),
            vec![(kind::CONTINUE, 1, BUFFER_PACKETS.to_le_bytes().to_vec())]
        );
        s.client_packet(&bridge::encode(kind::DATA, 1, b"hello"));
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(5);
        while !got.iter().any(|(k, _, _)| *k == kind::CLOSE) && Instant::now() < until {
            got.extend(poll_for(&mut s));
        }
        server.join().unwrap();
        assert!(
            got.contains(&(kind::CONTINUE, 1, BUFFER_PACKETS.to_le_bytes().to_vec())),
            "the credit is renewed once the DATA is written: {got:?}"
        );
        let data: Vec<u8> = got
            .iter()
            .filter(|(k, _, _)| *k == kind::DATA)
            .flat_map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(data, b"world");
        assert_eq!(got.last(), Some(&(kind::CLOSE, 1, vec![reason::VOLUNTARY])));
        assert_eq!(s.open_streams(), 0);
    }

    #[test]
    fn a_refused_connect_and_a_blocked_one_close_with_their_reasons() {
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let mut s = WispServer::new(&[route(80, port)]);
        s.take();
        s.client_packet(&bridge::connect(3, 80, HOST_NAME));
        s.client_packet(&bridge::connect(4, 443, HOST_NAME));
        s.client_packet(&bridge::connect(5, 80, "example.com"));
        assert_eq!(
            decoded(s.take()),
            vec![
                (kind::CLOSE, 3, vec![reason::REFUSED]),
                (kind::CLOSE, 4, vec![reason::BLOCKED]),
                (kind::CLOSE, 5, vec![reason::BLOCKED]),
            ]
        );
    }

    // The browser carrier over loopback: a real daemon, a page played by a WebSocket client.

    use crate::auth::{Auth, Token};
    use crate::daemon::Shutdown;
    use crate::http::Server;
    use crate::pool::Pool;
    use pemu_api::spec::CapsGroup;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    type Socket = tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>;

    fn token() -> Token {
        Token::from_bytes([0x5c; 32])
    }

    fn serve() -> u16 {
        let bound = crate::daemon::bind(0).expect("port 0 always binds");
        let port = bound.port;
        let server = Arc::new(Server::new(
            Auth::new(token(), port),
            Arc::new(Pool::new(1)),
            Shutdown::new(),
            BTreeSet::from([CapsGroup::Core]),
        ));
        let listener = bound.listener;
        std::thread::spawn(move || {
            let _ = crate::http::serve_blocking(listener, server);
        });
        port
    }

    fn request(port: u16, bearer: Option<&str>) -> tungstenite::handshake::client::Request {
        use tungstenite::client::IntoClientRequest;
        let mut request = format!("ws://127.0.0.1:{port}{RELAY_PATH}")
            .into_client_request()
            .expect("a client request");
        let headers = request.headers_mut();
        if let Some(bearer) = bearer {
            headers.insert(
                "authorization",
                format!("Bearer {bearer}").parse().expect("a header value"),
            );
        }
        headers.insert(
            "origin",
            format!("http://127.0.0.1:{port}")
                .parse()
                .expect("a header value"),
        );
        request
    }

    /// Opens a relay socket with the bearer credential and sends `hello`, returning the answer
    /// unread so a test can assert a refusal too.
    fn open(port: u16, hello: serde_json::Value) -> (Socket, serde_json::Value) {
        let (mut socket, _) = tungstenite::connect(request(port, Some(&token().to_hex())))
            .expect("the relay upgrade");
        socket
            .send(tungstenite::Message::text(hello.to_string()))
            .expect("send");
        let answer = match socket.read().expect("an answer") {
            tungstenite::Message::Text(text) => serde_json::from_str(text.as_str()).expect("JSON"),
            other => panic!("expected a text frame, got {other:?}"),
        };
        (socket, answer)
    }

    fn read_packets(socket: &mut Socket, want: usize) -> Vec<(u8, u32, Vec<u8>)> {
        if let tungstenite::stream::MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .expect("a read timeout, so a wedged relay fails rather than hangs");
        }
        let mut out = Vec::new();
        while out.len() < want {
            match socket.read() {
                Ok(tungstenite::Message::Binary(bytes)) => {
                    let (k, s, b) = bridge::decode(&bytes).expect("a WISP packet");
                    out.push((k, s, b.to_vec()));
                }
                Ok(_) => {}
                Err(err) => panic!("after {out:?}: {err}"),
            }
        }
        out
    }

    #[test]
    fn the_relay_upgrade_needs_the_daemon_credential() {
        let port = serve();
        let refused = tungstenite::connect(request(port, None));
        match refused {
            Err(tungstenite::Error::Http(response)) => {
                assert_eq!(
                    response.status(),
                    401,
                    "an unauthenticated relay is refused"
                )
            }
            other => panic!("expected a 401, got {other:?}"),
        }
        let (_socket, answer) = open(
            port,
            serde_json::json!({ "hello": RELAY_HELLO, "routes": [] }),
        );
        assert_eq!(answer["ready"], serde_json::Value::Bool(true));
    }

    #[test]
    fn a_relay_socket_carries_a_stream_to_a_host_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host_port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 5];
            sock.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"hello");
            sock.write_all(b"world").unwrap();
        });
        let port = serve();
        let (mut socket, ready) = open(
            port,
            serde_json::json!({
                "hello": RELAY_HELLO,
                "routes": [{ "port": 80, "host_port": host_port }],
            }),
        );
        assert_eq!(ready["ready"], serde_json::Value::Bool(true));
        assert_eq!(ready["buffer"], serde_json::json!(BUFFER_PACKETS));
        assert_eq!(
            ready["routes"][0]["host_port"],
            serde_json::json!(host_port)
        );

        assert_eq!(
            read_packets(&mut socket, 1),
            vec![(kind::CONTINUE, 0, BUFFER_PACKETS.to_le_bytes().to_vec())]
        );
        for packet in [
            bridge::connect(1, 80, HOST_NAME),
            bridge::encode(kind::DATA, 1, b"hello"),
        ] {
            socket
                .send(tungstenite::Message::binary(packet))
                .expect("send");
        }
        let mut got = Vec::new();
        while !got.iter().any(|(k, _, _)| *k == kind::CLOSE) {
            got.extend(read_packets(&mut socket, 1));
        }
        server.join().unwrap();
        assert!(
            got.contains(&(kind::CONTINUE, 1, BUFFER_PACKETS.to_le_bytes().to_vec())),
            "the stream's own credit is the `connected` the LAN waits for: {got:?}"
        );
        let data: Vec<u8> = got
            .iter()
            .filter(|(k, _, _)| *k == kind::DATA)
            .flat_map(|(_, _, b)| b.clone())
            .collect();
        assert_eq!(data, b"world", "{got:?}");
        assert_eq!(got.last(), Some(&(kind::CLOSE, 1, vec![reason::VOLUNTARY])));
    }

    #[test]
    fn a_hello_is_refused_by_name_and_the_socket_closes() {
        let port = serve();
        for (hello, expected) in [
            (
                serde_json::json!({ "hello": RELAY_HELLO, "routes": [{ "port": 80, "host_port": port }] }),
                "host port",
            ),
            (
                serde_json::json!({ "hello": "passportsim-browser", "routes": [] }),
                "the first frame must be",
            ),
            (
                serde_json::json!({ "hello": RELAY_HELLO }),
                "`routes` is an array",
            ),
            (
                serde_json::json!({ "hello": RELAY_HELLO, "routes": [{ "port": 0 }] }),
                "a route needs `port`",
            ),
        ] {
            let (mut socket, answer) = open(port, hello.clone());
            let error = answer["error"].as_str().unwrap_or_default().to_string();
            assert!(
                error.contains(expected),
                "{hello}: expected a refusal naming `{expected}`, got {answer}"
            );
            assert!(
                matches!(socket.read(), Ok(tungstenite::Message::Close(_)) | Err(_)),
                "{hello}: the socket closes after the refusal"
            );
        }
    }

    fn pool_with_p1() -> (pemu_api::commands::start::Pool, InstanceId) {
        let mut pool = pemu_api::commands::start::Pool::new();
        let machine = pemu_testkit::mock_machine::MockScript::new().build();
        let id = pool.attach(
            &pemu_api::commands::start::StartArgs::default(),
            Box::new(machine),
        );
        (pool, id)
    }

    fn routes_of(session: &Session) -> Option<Vec<NetRoute>> {
        let id = session.id;
        session.with_table(|natives: &mut Natives| {
            natives.0.get(&id).map(|n| n.server.routes().to_vec())
        })
    }

    fn has_pacer(session: &Session) -> bool {
        let id = session.id;
        session.with_table(|pacers: &mut Pacers| pacers.0.contains_key(&id))
    }

    #[test]
    fn two_pools_that_mint_the_same_id_keep_their_own_bridges_and_pacers() {
        let (mut first, a) = pool_with_p1();
        let (mut second, b) = pool_with_p1();
        assert_eq!(a, b, "every new pool mints `p1` first");

        attach(first.session_mut(a).expect("live"), &[route(8080, 18080)]).expect("attaches");
        let session = second.session_mut(b).expect("live");
        assert_eq!(
            native_streams(session),
            None,
            "no bridge in the second pool yet"
        );
        attach(session, &[route(9090, 19090)]).expect("attaches");
        assert_eq!(routes_of(session), Some(vec![route(9090, 19090)]));
        assert_eq!(
            routes_of(first.session_mut(a).expect("live")),
            Some(vec![route(8080, 18080)]),
            "the second attach replaced nothing of the first pool's"
        );
        detach(second.session_mut(b).expect("live"));
        assert_eq!(native_streams(second.session_mut(b).expect("live")), None);
        assert_eq!(
            native_streams(first.session_mut(a).expect("live")),
            Some(0),
            "the second pool's detach left the first one's bridge"
        );

        tick(first.session_mut(a).expect("live"), true);
        assert!(
            !has_pacer(first.session_mut(a).expect("live")),
            "a native bridge paces itself"
        );
        tick(second.session_mut(b).expect("live"), true);
        assert!(has_pacer(second.session_mut(b).expect("live")));
        detach(first.session_mut(a).expect("live"));
        assert!(!has_pacer(first.session_mut(a).expect("live")));
        tick(first.session_mut(a).expect("live"), true);
        assert!(has_pacer(first.session_mut(a).expect("live")));

        attach(second.session_mut(b).expect("live"), &[route(9091, 19091)]).expect("attaches");
        drop(first);
        let session = second.session_mut(b).expect("live");
        assert_eq!(routes_of(session), Some(vec![route(9091, 19091)]));
        assert!(
            has_pacer(session),
            "dropping the other pool leaves this one's pacer"
        );
    }
}
