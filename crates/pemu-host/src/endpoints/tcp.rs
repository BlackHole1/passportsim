//! The TCP endpoint: one loopback listener on an auto-assigned port, the cross-host way `esptool`,
//! `idf.py` and a raw console reach an instance's USB Serial/JTAG.
//!
//! - `rfc2217://127.0.0.1:<port>` is the flashing form: DTR and RTS arrive as RFC 2217
//!   `SET-CONTROL` and become `UsbLine` inputs, so both esptool reset sequences work.
//! - `socket://127.0.0.1:<port>` is monitor and log-only, since esptool forces `no_reset` there;
//!   with [`TcpOptions::auto_download`] the endpoint resets into download mode on the first `SYNC`
//!   and hard-resets when the client disconnects.
//! - a client silent for 300 ms is a raw console ([`super::detect`]).
//!
//! One client at a time, like a USB CDC port: a second connection waits in the backlog. The
//! endpoint never opens a host serial device.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use super::detect::{Mode, SILENCE, classify};
use super::live::UsjLink;
use super::rfc2217::{Event, LineState, ResetBridge, Rfc2217, escape_into};
use super::slip::SyncWatch;
use crate::daemon::BIND_ADDR;

const POLL: Duration = Duration::from_millis(20);

/// How long one blocked socket write waits before rechecking the stop flag, so a client that stops
/// reading stalls only its own output.
const WRITE_POLL: Duration = Duration::from_millis(100);

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct TcpOptions {
    /// `--auto-download`: in SLIP mode, reset into ROM download mode on the first `SYNC`, and hard
    /// reset when the client disconnects.
    pub auto_download: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TcpStatus {
    pub connections: u64,
    pub mode: Option<Mode>,
    pub last_mode: Option<Mode>,
}

/// A running TCP endpoint. Dropping it stops the listener.
#[derive(Debug)]
pub struct TcpEndpoint {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    status: Arc<Mutex<TcpStatus>>,
    /// The connected client's socket, so closing can shut it down under a blocked writer.
    current: Current,
    thread: Option<JoinHandle<()>>,
}

type Current = Arc<Mutex<Option<TcpStream>>>;

impl TcpEndpoint {
    pub fn bind(link: Arc<UsjLink>, opts: TcpOptions) -> io::Result<TcpEndpoint> {
        let listener = TcpListener::bind((BIND_ADDR, 0))?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        // A bridge may never reach an endpoint port.
        crate::relay_wisp::deny_port(addr.port());
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(TcpStatus::default()));
        let current: Current = Arc::default();
        let thread = std::thread::Builder::new()
            .name(format!("usj-tcp-{}", addr.port()))
            .spawn({
                let stop = Arc::clone(&stop);
                let status = Arc::clone(&status);
                let current = Arc::clone(&current);
                move || {
                    // This thread carries a client's bytes to a live guest.
                    crate::platform::machine_thread();
                    accept_loop(&listener, &link, opts, &stop, &status, &current)
                }
            })?;
        Ok(TcpEndpoint {
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

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn rfc2217_url(&self) -> String {
        format!("rfc2217://127.0.0.1:{}", self.port())
    }

    pub fn socket_url(&self) -> String {
        format!("socket://127.0.0.1:{}", self.port())
    }

    pub fn status(&self) -> TcpStatus {
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
        // Unblocks a writer stuck on a client that does not read, and ends the reader.
        if let Some(stream) = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for TcpEndpoint {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(
    listener: &TcpListener,
    link: &Arc<UsjLink>,
    opts: TcpOptions,
    stop: &AtomicBool,
    status: &Mutex<TcpStatus>,
    current: &Mutex<Option<TcpStream>>,
) {
    while !stop.load(Ordering::SeqCst) && !link.stopped() {
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                status.lock().unwrap_or_else(|e| e.into_inner()).connections += 1;
                if let Ok(clone) = stream.try_clone() {
                    *current.lock().unwrap_or_else(|e| e.into_inner()) = Some(clone);
                }
                let _ = serve_client(stream, link, opts, stop, status);
                current.lock().unwrap_or_else(|e| e.into_inner()).take();
                let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
                s.mode = None;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

fn first_byte(stream: &mut TcpStream, stop: &AtomicBool) -> io::Result<Option<Option<u8>>> {
    stream.set_read_timeout(Some(POLL))?;
    let start = std::time::Instant::now();
    let mut byte = [0u8; 1];
    while start.elapsed() < SILENCE {
        if stop.load(Ordering::SeqCst) {
            return Ok(None);
        }
        match stream.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(Some(byte[0]))),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(Some(None))
}

/// The line sequence that resets into download mode: (RTS 0, DTR 1) sets the flag, (RTS 1, DTR 0)
/// resets with it, (0, 0) clears it for the next reset.
pub const DOWNLOAD_RESET: [LineState; 4] = [
    LineState {
        dtr: false,
        rts: false,
    },
    LineState {
        dtr: true,
        rts: false,
    },
    LineState {
        dtr: false,
        rts: true,
    },
    LineState {
        dtr: false,
        rts: false,
    },
];

pub const HARD_RESET: [LineState; 3] = [
    LineState {
        dtr: false,
        rts: false,
    },
    LineState {
        dtr: false,
        rts: true,
    },
    LineState {
        dtr: false,
        rts: false,
    },
];

fn serve_client(
    mut stream: TcpStream,
    link: &Arc<UsjLink>,
    opts: TcpOptions,
    stop: &AtomicBool,
    status: &Mutex<TcpStatus>,
) -> io::Result<()> {
    // The client counts as a connected host tool from its accept, so the instance runs in wall
    // time; dropping the tap on any exit detaches it.
    let tap = link.attach();
    // The tap moves into the output thread, so keep its id: a `PURGE-DATA` from this client clears
    // only this client's queue.
    let tap_id = tap.id();
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_POLL))?;
    let Some(first) = first_byte(&mut stream, stop)? else {
        return Ok(());
    };
    let mode = classify(first);
    {
        let mut s = status.lock().unwrap_or_else(|e| e.into_inner());
        s.mode = Some(mode);
        s.last_mode = Some(mode);
    }
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    let done = Arc::new(AtomicBool::new(false));
    let pump = std::thread::Builder::new()
        .name("usj-tcp-out".to_owned())
        .spawn({
            let link = Arc::clone(link);
            let writer = Arc::clone(&writer);
            let done = Arc::clone(&done);
            move || {
                crate::platform::machine_thread();
                output_pump(&link, &tap, &writer, mode, &done);
                drop(tap);
            }
        })?;

    let mut rfc = Rfc2217::new(link.line());
    // What the client sets is not what the chip gets: `rfc2217::ResetBridge` translates resets.
    let mut bridge = ResetBridge::new(link.line());
    let mut watch = SyncWatch::new();
    let mut download_reset_sent = false;
    let result = (|| -> io::Result<()> {
        let mut pending: Vec<u8> = first.into_iter().collect();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            if !pending.is_empty() {
                match mode {
                    Mode::Rfc2217 => {
                        let fed = rfc.feed(&pending);
                        // Apply the events before the answer: pyserial returns from a purge at the
                        // ack and then drains its buffer, so an earlier ack would let the output
                        // thread refill the tap in between. A full to-guest queue therefore delays
                        // the ack, as a full OUT endpoint would.
                        for event in fed.events {
                            match event {
                                Event::Data(data) => {
                                    if !link.send_bytes(&data) {
                                        return Ok(());
                                    }
                                }
                                Event::Line(line) => {
                                    for row in bridge.client(line) {
                                        link.set_line(row);
                                    }
                                }
                                // Each half of `PURGE-DATA` is its own queue; the guest-to-client
                                // half is this transport's alone.
                                Event::Purge(direction) => {
                                    if direction.clears_to_guest() {
                                        link.purge_to_guest();
                                    }
                                    if direction.clears_from_guest() {
                                        link.purge_tap(tap_id);
                                    }
                                }
                            }
                        }
                        if !fed.reply.is_empty() {
                            write_until(&writer, &fed.reply, || stop.load(Ordering::SeqCst))?;
                        }
                    }
                    Mode::Slip => {
                        let syncs = watch.feed(&pending);
                        if !link.send_bytes(&pending) {
                            return Ok(());
                        }
                        if opts.auto_download && syncs > 0 && !download_reset_sent {
                            download_reset_sent = true;
                            for line in DOWNLOAD_RESET {
                                link.set_line(line);
                            }
                        }
                    }
                    Mode::Console => {
                        if !link.send_bytes(&pending) {
                            return Ok(());
                        }
                    }
                }
                pending.clear();
            }
            if stop.load(Ordering::SeqCst) || link.stopped() {
                return Ok(());
            }
            match stream.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => return Err(e),
            }
        }
    })();
    done.store(true, Ordering::SeqCst);
    let _ = pump.join();
    for row in bridge.close() {
        link.set_line(row);
    }
    if mode == Mode::Slip && opts.auto_download {
        for line in HARD_RESET {
            link.set_line(line);
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    result
}

fn output_pump(
    link: &UsjLink,
    tap: &super::live::Tap,
    writer: &Mutex<TcpStream>,
    mode: Mode,
    done: &AtomicBool,
) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut escaped = Vec::with_capacity(32 * 1024);
    while !done.load(Ordering::SeqCst) {
        let n = match link.read_output(tap, &mut buf, POLL) {
            None => return,
            Some(0) => continue,
            Some(n) => n,
        };
        let bytes: &[u8] = match mode {
            Mode::Rfc2217 => {
                escaped.clear();
                escape_into(&buf[..n], &mut escaped);
                &escaped
            }
            _ => &buf[..n],
        };
        if write_until(writer, bytes, || done.load(Ordering::SeqCst)).is_err() {
            return;
        }
    }
}

/// Writes all of `bytes`, taking the writer lock per attempt so the reply writer and the output
/// pump never wait on each other's blocked write; `Interrupted` once `give_up` says so. A
/// timed-out attempt is backpressure, not an error.
fn write_until(
    writer: &Mutex<TcpStream>,
    mut bytes: &[u8],
    give_up: impl Fn() -> bool,
) -> io::Result<()> {
    while !bytes.is_empty() {
        if give_up() {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let written = writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write(bytes);
        match written {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => bytes = &bytes[n..],
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::live::echo::EchoMachine;
    use super::super::live::{LiveRunner, Pacing};
    use super::super::rfc2217::{IAC, OPT_COM_PORT, SB, SE, WILL, cmd, control};

    fn read_until(stream: &mut TcpStream, want: usize) -> Vec<u8> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while got.len() < want {
            let n = stream.read(&mut buf).expect("the endpoint answers in time");
            assert!(n > 0, "the endpoint closed early");
            got.extend_from_slice(&buf[..n]);
        }
        got
    }

    #[test]
    fn the_listener_binds_loopback_on_an_auto_assigned_port() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        assert!(ep.addr().ip().is_loopback());
        assert_ne!(ep.port(), 0);
        assert_eq!(
            ep.rfc2217_url(),
            format!("rfc2217://127.0.0.1:{}", ep.port())
        );
        ep.close();
    }

    #[test]
    fn an_rfc2217_client_gets_replies_its_lines_reach_the_guest_and_data_is_escaped() {
        let guest = EchoMachine::new();
        let lines = Arc::clone(&guest.lines);
        let runner = LiveRunner::start(guest, Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        let mut c = TcpStream::connect(ep.addr()).expect("connect");
        c.write_all(&[IAC, WILL, OPT_COM_PORT]).expect("write");
        c.write_all(&[
            IAC,
            SB,
            OPT_COM_PORT,
            cmd::SET_CONTROL,
            control::DTR_ON,
            IAC,
            SE,
        ])
        .expect("write");
        let reply = read_until(&mut c, 3 + 7);
        assert_eq!(&reply[..3], &[IAC, 253, OPT_COM_PORT]);
        assert_eq!(
            &reply[3..7],
            &[IAC, SB, OPT_COM_PORT, cmd::SET_CONTROL + 100]
        );
        assert_eq!(reply[7], control::DTR_ON);
        // 0xFF must be doubled on the wire; the echo guest leaves it as it is.
        c.write_all(&[b'a', IAC, IAC]).expect("write");
        let echoed = read_until(&mut c, 3);
        assert_eq!(echoed, vec![b'A', IAC, IAC]);
        assert_eq!(ep.status().mode, Some(Mode::Rfc2217));
        drop(c);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lines.lock().expect("lines").is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(POLL);
        }
        assert_eq!(*lines.lock().expect("lines"), vec![(true, false)]);
        ep.close();
        runner.stop();
    }

    #[test]
    fn close_returns_while_a_client_does_not_read() {
        let mut guest = EchoMachine::new();
        guest.flood = 64 * 1024;
        let runner = LiveRunner::start(guest, Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        let client = TcpStream::connect(ep.addr()).expect("connect");
        std::thread::sleep(SILENCE + Duration::from_millis(700));
        let (done, closed) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        std::thread::spawn(move || {
            ep.close();
            let _ = done.send(());
        });
        closed
            .recv_timeout(Duration::from_secs(2))
            .expect("close returns within 2 s while the client does not read");
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(client);
        runner.stop();
    }

    #[test]
    fn a_silent_client_is_a_raw_console() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        let mut c = TcpStream::connect(ep.addr()).expect("connect");
        std::thread::sleep(SILENCE + Duration::from_millis(100));
        c.write_all(b"ping").expect("write");
        assert_eq!(read_until(&mut c, 4), b"PING");
        assert_eq!(ep.status().last_mode, Some(Mode::Console));
        ep.close();
    }

    #[test]
    fn auto_download_resets_on_sync_and_hard_resets_on_disconnect() {
        let guest = EchoMachine::new();
        let lines = Arc::clone(&guest.lines);
        let runner = LiveRunner::start(guest, Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = TcpEndpoint::bind(
            runner.link(),
            TcpOptions {
                auto_download: true,
            },
        )
        .expect("bind");
        let mut c = TcpStream::connect(ep.addr()).expect("connect");
        let mut sync = vec![
            0xC0, 0x00, 0x08, 0x24, 0x00, 0, 0, 0, 0, 0x07, 0x07, 0x12, 0x20,
        ];
        sync.extend([0x55; 32]);
        sync.push(0xC0);
        c.write_all(&sync).expect("write");
        c.write_all(&sync).expect("write");
        let _ = read_until(&mut c, 1);
        drop(c);
        let want: Vec<(bool, bool)> = DOWNLOAD_RESET
            .iter()
            .chain(HARD_RESET.iter())
            .map(|l| (l.dtr, l.rts))
            .collect();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while lines.lock().expect("lines").len() < want.len()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(POLL);
        }
        assert_eq!(
            *lines.lock().expect("lines"),
            want,
            "one download reset, one hard reset"
        );
        ep.close();
    }
}
