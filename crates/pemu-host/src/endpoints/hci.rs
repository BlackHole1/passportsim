//! The external HCI transport: an H4 stream over a loopback socket, one peer at a time, carrying
//! the BLE bridge's packets. It lives here because `pemu-api` opens no socket; `env --ble-bridge
//! attach` reaches it through [`pemu_api::commands::ble_scan::BridgeIo`].
//!
//! Towards the peer go the guest's host-to-controller packets, read from the bridge window by
//! cursor without changing guest state; towards the guest come the peer's packets, each journaled
//! as `InputEvent::HciPacket` with a stream sequence number, so the run replays and losses are
//! counted. A second connection waits in the backlog, and a peer that sends an undefined indicator
//! has lost framing and is dropped.
//!
//! Nothing here runs the machine; while the bridge is live, pacing is held at `Wall { rate: 1 }`.
//! The pump reaches its instance through the pool that opened the bridge ([`PoolReach`]), since an
//! `InstanceId` is only unique within its pool.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use pemu_api::commands::ble_scan::{H4Stream, bridge_inbound, bridge_outbound};
use pemu_api::commands::start::{PoolReach, Reached, Session};
use pemu_api::instance::InstanceId;

use crate::daemon::BIND_ADDR;

const POLL: Duration = Duration::from_millis(20);

/// How often the pump polls the bridge window while the socket is quiet: the guest writes at
/// virtual instants no host timer knows, and 5 ms is far below the guest's 2 s HCI timeout.
const PUMP: Duration = Duration::from_millis(5);

/// How many peer packets may wait for the session to return to the pool before the connection is
/// dropped rather than buffering without end.
const MAX_PENDING: usize = 256;

const WRITE_WAIT: Duration = Duration::from_millis(500);

const READ_CHUNK: usize = 4096;

/// A peer sending more than this without completing a packet is not speaking H4; the largest
/// possible packet is far below it.
const MAX_BUFFERED: usize = 128 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BridgeStatus {
    pub connections: u64,
    pub inbound: u64,
    pub outbound: u64,
    pub connected: bool,
}

/// A running transport. Dropping it stops the listener.
#[derive(Debug)]
pub struct HciBridge {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<BridgeStatus>>,
    current: Arc<Mutex<Option<TcpStream>>>,
    thread: Option<JoinHandle<()>>,
}

impl HciBridge {
    /// Binds 127.0.0.1 on an auto-assigned port and pumps packets between it and instance `id` of
    /// the pool `pool` reaches.
    pub fn bind(id: InstanceId, pool: PoolReach) -> io::Result<HciBridge> {
        let target = Target { id, pool };
        let listener = TcpListener::bind((BIND_ADDR, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        // A listener of this process is never a Wi-Fi bridge destination.
        crate::relay_wisp::deny_port(addr.port());
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(BridgeStatus::default()));
        let current: Arc<Mutex<Option<TcpStream>>> = Arc::default();
        let thread = std::thread::Builder::new()
            .name(format!("hci-bridge-{}", addr.port()))
            .spawn({
                let stop = Arc::clone(&stop);
                let status = Arc::clone(&status);
                let current = Arc::clone(&current);
                move || {
                    crate::platform::machine_thread();
                    accept_loop(&listener, &target, &stop, &status, &current)
                }
            })?;
        Ok(HciBridge {
            addr,
            stop,
            status,
            current,
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The URL a peer connects to. `hci://` names the payload; the socket is plain TCP.
    pub fn url(&self) -> String {
        format!("hci://127.0.0.1:{}", self.addr.port())
    }

    pub fn status(&self) -> BridgeStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(stream) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            // The pump holds its pool while it takes a turn, so this bridge can be dropped on the
            // pump's own thread, which must not join itself. The loop ends on the stop flag.
            if t.thread().id() != std::thread::current().id() {
                let _ = t.join();
            }
        }
    }
}

struct Target {
    id: InstanceId,
    pool: PoolReach,
}

impl Drop for HciBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: &TcpListener,
    target: &Target,
    stop: &AtomicBool,
    status: &Mutex<BridgeStatus>,
    current: &Mutex<Option<TcpStream>>,
) {
    // The cursor outlives one connection, so a reconnecting peer gets what the guest sent while
    // nobody listened, as far back as the window holds.
    let cursor = AtomicU64::new(0);
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                {
                    let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                    s.connections += 1;
                    s.connected = true;
                }
                if let Ok(clone) = stream.try_clone() {
                    *current.lock().unwrap_or_else(|e| e.into_inner()) = Some(clone);
                }
                let _ = serve(stream, target, &cursor, stop, status);
                current.lock().unwrap_or_else(|e| e.into_inner()).take();
                status.lock().unwrap_or_else(|e| e.into_inner()).connected = false;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

fn serve(
    mut stream: TcpStream,
    target: &Target,
    cursor: &AtomicU64,
    stop: &AtomicBool,
    status: &Mutex<BridgeStatus>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(PUMP))?;
    // A peer that stopped reading makes the write fail and ends the connection, as a vanished
    // controller would.
    stream.set_write_timeout(Some(WRITE_WAIT))?;
    stream.set_nodelay(true)?;
    // Packets for an instance checked out to a command wait here: a driven instance is out of the
    // pool most of the time, and blocking the reader while the peer still writes would deadlock
    // two blocking sockets.
    let mut pending: std::collections::VecDeque<(u64, Vec<u8>)> = std::collections::VecDeque::new();
    let mut framer = H4Stream::new();
    let mut buf = [0u8; READ_CHUNK];
    // The sequence continues from the packets already delivered, so a reconnect does not look
    // like a loss.
    let mut seq = inbound_so_far(target);
    while !stop.load(Ordering::SeqCst) {
        match stream.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let packets = match framer.feed(&buf[..n]) {
                    Ok(packets) => packets,
                    // An indicator no transport defines: framing is lost for good.
                    Err(_) => return Ok(()),
                };
                if framer.buffered() > MAX_BUFFERED {
                    return Ok(());
                }
                for packet in packets {
                    pending.push_back((seq, packet));
                    seq += 1;
                }
                if pending.len() > MAX_PENDING {
                    return Ok(());
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(_) => return Ok(()),
        }
        // One pool turn per loop: an instance in the pool takes the whole queue, one that is out
        // leaves it for the next turn.
        match deliver(target, &mut pending) {
            Delivered::Gone => return Ok(()),
            Delivered::Some(n) => {
                if n > 0 {
                    status.lock().unwrap_or_else(|e| e.into_inner()).inbound += n;
                }
            }
        }
        let taken = take_outbound(target, cursor);
        for packet in &taken {
            if stream.write_all(packet).is_err() {
                return Ok(());
            }
        }
        if !taken.is_empty() {
            let _ = stream.flush();
            status.lock().unwrap_or_else(|e| e.into_inner()).outbound += taken.len() as u64;
        }
    }
    Ok(())
}

enum Delivered {
    Some(u64),
    Gone,
}

/// Journals everything waiting, oldest first, in one turn of the pool, never blocking. Packets for
/// a checked-out instance stay in `pending`; a lost instance or a refused input ends the
/// connection. Order is kept, because an out-of-order HCI stream is worse than a late one.
fn deliver(target: &Target, pending: &mut std::collections::VecDeque<(u64, Vec<u8>)>) -> Delivered {
    if pending.is_empty() {
        return Delivered::Some(0);
    }
    let id = target.id;
    // Never a blocking lock: `env --ble-bridge detach` joins this thread from inside a command
    // that holds the pool, so waiting here would deadlock.
    let reached = target.pool.try_with(|pool| {
        let Some(session) = pool.session_mut(id) else {
            return if pool.table().get(id).is_some() {
                Delivered::Some(0)
            } else {
                Delivered::Gone
            };
        };
        let mut done = 0u64;
        while let Some((seq, data)) = pending.pop_front() {
            if bridge_inbound(session, seq, data).is_err() {
                return Delivered::Gone;
            }
            done += 1;
        }
        Delivered::Some(done)
    });
    match reached {
        Reached::Ran(delivered) => delivered,
        Reached::Busy => Delivered::Some(0),
        Reached::Gone => Delivered::Gone,
    }
}

fn take_outbound(target: &Target, cursor: &AtomicU64) -> Vec<Vec<u8>> {
    let at = cursor.load(Ordering::SeqCst);
    let taken = target.pool.try_with(|pool| {
        let session = pool.session_mut(target.id)?;
        bridge_outbound(session, at).ok()
    });
    match taken {
        Reached::Ran(Some((packets, next, _dropped))) => {
            cursor.store(next, Ordering::SeqCst);
            packets
        }
        _ => Vec::new(),
    }
}

// The `BridgeIo` a host installs.

/// The open bridges of one pool's instances, one per instance, as a table of that pool (ids are
/// only unique within a pool). Dropping the pool stops each listener and joins its thread.
#[derive(Debug, Default)]
struct Bridges(std::collections::BTreeMap<InstanceId, HciBridge>);

pub fn install() {
    pemu_api::commands::ble_scan::set_bridge_io(pemu_api::commands::ble_scan::BridgeIo {
        attach,
        detach,
    });
}

/// Opens a listener for the session's instance and answers its URL. A pool no host thread can reach
/// is refused rather than served through another pool.
fn attach(session: &Session) -> Result<String, String> {
    let id = session.id;
    let pool = session.reach().ok_or_else(|| {
        format!(
            "instance `{id}` belongs to a pool no transport thread can reach (only the process \
             pool, or one made with `Pool::into_shared`), so a bridge could not deliver to it"
        )
    })?;
    session.with_table(|bridges: &mut Bridges| {
        if let Some(bridge) = bridges.0.get(&id) {
            // Attaching an open bridge returns the same bridge; the peer keeps its connection.
            return Ok(bridge.url());
        }
        let bridge = HciBridge::bind(id, pool).map_err(|e| e.to_string())?;
        let url = bridge.url();
        bridges.0.insert(id, bridge);
        Ok(url)
    })
}

fn detach(session: &Session) -> Result<(), String> {
    let id = session.id;
    // Closed outside the table's lock: closing joins the pump thread.
    let bridge = session.with_table(|bridges: &mut Bridges| bridges.0.remove(&id));
    if let Some(bridge) = bridge {
        bridge.close();
    }
    Ok(())
}

pub fn status(session: &Session) -> Option<BridgeStatus> {
    let id = session.id;
    session.with_table(|bridges: &mut Bridges| bridges.0.get(&id).map(HciBridge::status))
}

/// The count of packets already taken from an external stream, which the next sequence number
/// continues from.
fn inbound_so_far(target: &Target) -> u64 {
    let reached = target.pool.try_with(|pool| {
        let Some(session) = pool.session_mut(target.id) else {
            return 0;
        };
        pemu_api::commands::ble_scan::ble_state(session)
            .map(|state| state.external.next_seq)
            .unwrap_or(0)
    });
    match reached {
        Reached::Ran(seq) => seq,
        Reached::Busy | Reached::Gone => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use pemu_api::commands::start::{Pool, StartArgs, with_pool};
    use pemu_machine::machine::At;
    use pemu_testkit::mock_machine::MockScript;

    /// A pool behind an `Arc` (reachable from a pump) with one instance on a scripted machine.
    fn pool_with_p1() -> (Arc<Mutex<Pool>>, InstanceId) {
        let pool = Pool::new().into_shared();
        let id = lock(&pool).attach(&StartArgs::default(), Box::new(MockScript::new().build()));
        (pool, id)
    }

    fn lock(pool: &Mutex<Pool>) -> std::sync::MutexGuard<'_, Pool> {
        pool.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn on<R>(pool: &Mutex<Pool>, id: InstanceId, f: impl FnOnce(&mut Session) -> R) -> R {
        f(lock(pool).session_mut(id).expect("live"))
    }

    fn port(url: &str) -> u16 {
        url.rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("a port")
    }

    fn listening(port: u16) -> bool {
        let addr = SocketAddr::from((BIND_ADDR, port));
        TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok()
    }

    #[test]
    fn two_pools_that_mint_the_same_id_keep_their_own_bridges() {
        let (first, a) = pool_with_p1();
        let (second, b) = pool_with_p1();
        assert_eq!(a, b, "every new pool mints `p1` first");

        let url_a = on(&first, a, |s| attach(s)).expect("binds");
        assert_eq!(
            on(&second, b, |s| status(s)),
            None,
            "no bridge in the second pool yet"
        );
        let url_b = on(&second, b, |s| attach(s)).expect("binds");
        assert_ne!(url_a, url_b, "each pool's `p1` has its own listener");
        assert_eq!(
            on(&first, a, |s| attach(s)).expect("the same bridge"),
            url_a,
            "a re-attach in the first pool answers the first pool's bridge"
        );

        on(&second, b, |s| detach(s)).expect("closes");
        assert_eq!(on(&second, b, |s| status(s)), None);
        assert!(on(&first, a, |s| status(s)).is_some());
        assert!(
            listening(port(&url_a)),
            "the second pool's detach left the first one's listener"
        );

        let url_b = on(&second, b, |s| attach(s)).expect("binds");
        drop(first);
        assert!(on(&second, b, |s| status(s)).is_some());
        assert!(
            listening(port(&url_b)),
            "dropping the other pool leaves this one's bridge serving"
        );
        on(&second, b, |s| detach(s)).expect("closes");
    }

    #[test]
    fn a_pool_no_thread_can_reach_is_refused_a_bridge() {
        let mut pool = Pool::new();
        let id = pool.attach(&StartArgs::default(), Box::new(MockScript::new().build()));
        let refused = attach(pool.session_mut(id).expect("live")).expect_err("refused");
        assert!(
            refused.contains("no transport thread can reach"),
            "{refused}"
        );
    }

    /// The process pool and a shared pool both hold an instance with the same id; a packet on the
    /// shared pool's bridge must land in the shared pool's instance only.
    #[test]
    fn a_bridge_delivers_to_the_pool_that_opened_it_and_not_to_the_process_pool() {
        let script = || Box::new(MockScript::new().build());
        let theirs = with_pool(|pool| pool.attach(&StartArgs::default(), script()));
        let hash = |session: &mut Session| session.snapshot_machine().state_hash();
        let theirs_before = with_pool(|pool| hash(pool.session_mut(theirs).expect("live")));

        let ours = Pool::new().into_shared();
        let id = loop {
            let id = lock(&ours).attach(&StartArgs::default(), script());
            if id == theirs {
                break id;
            }
        };
        let ours_before = on(&ours, id, hash);
        let url = on(&ours, id, |s| attach(s)).expect("binds");

        let mut peer = TcpStream::connect(SocketAddr::from((BIND_ADDR, port(&url)))).expect("peer");
        // An HCI Command Complete event for Reset (H4 indicator 0x04, Core Vol 4 Part E 7.7.14).
        peer.write_all(&[0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00])
            .expect("sent");
        let deadline = Instant::now() + Duration::from_secs(5);
        while on(&ours, id, hash) == ours_before {
            assert!(
                Instant::now() < deadline,
                "the packet never reached the pool that opened the bridge"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let probe = on(&ours, id, |s| {
            s.machine().input(
                At::Now,
                pemu_core::input::InputEvent::HciPacket {
                    seq: 99,
                    data: Vec::new(),
                },
            )
        });
        assert_eq!(probe, Ok(1));
        assert_eq!(
            with_pool(|pool| hash(pool.session_mut(theirs).expect("live"))),
            theirs_before,
            "the process pool's instance with the same id got nothing"
        );

        drop(peer);
        on(&ours, id, |s| detach(s)).expect("closes");
        let _ = with_pool(|pool| pool.destroy(theirs));
    }
}
