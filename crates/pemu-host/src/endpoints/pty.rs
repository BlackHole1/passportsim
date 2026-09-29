//! A pseudo-terminal carrying an instance's USB Serial/JTAG bytes, for tools that insist on a
//! device path. macOS only; elsewhere `serve` refuses `--pty` with `E_HOST_UNSUPPORTED`.
//!
//! Data only: a macOS pty answers every modem-control ioctl (`TIOCMGET`, `TIOCMBIS`, `TIOCMBIC`,
//! `TIOCMSET`) with `ENOTTY`, so DTR and RTS cannot cross it and flashing uses `rfc2217://`
//! ([`LIMITATION`], [`modem_lines_supported`]). It also rejects 460800 and 921600 baud and buffers
//! about 1 KB per direction, so output waits in the [`UsjLink`] while nobody reads the slave.
//!
//! The libSystem functions are declared by hand rather than adding a `libc` dependency; each is
//! used under its POSIX contract, noted at the call.

use std::ffi::CStr;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::live::{Tap, UsjLink};

/// The documented limitation `status` reports.
pub const LIMITATION: &str = "data only: a macOS pty has no modem-control lines (TIOCMGET and \
     TIOCMSET fail with ENOTTY), so DTR and RTS cannot reset the instance; flash over \
     rfc2217://127.0.0.1:<port>, and use 115200 or 230400 baud";

const O_RDWR: i32 = 0x0002;
/// `O_NOCTTY` (`sys/fcntl.h`): opening the slave must not make it this process's terminal.
const O_NOCTTY: i32 = 0x0002_0000;
/// `O_CLOEXEC` (`sys/fcntl.h`): a child the host spawns must not inherit the master.
const O_CLOEXEC: i32 = 0x0100_0000;
#[cfg(test)]
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const FD_CLOEXEC: i32 = 1;
const EINVAL: i32 = 22;
const TCSANOW: i32 = 0;
/// `TIOCMGET`, `_IOR('t', 106, int)` (`sys/ttycom.h`).
const TIOCMGET: u64 = 0x4004_746A;
pub const ENOTTY: i32 = 25;
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLL_MS: i32 = 20;
/// How long [`PtyEndpoint::shutdown`] waits for a pump thread before abandoning it. A pump can be
/// wedged past it: [`pump_out`] writes the master blocking, and an unread slave queue blocks that
/// write, so waiting would turn a test that panics with bytes queued into a hang.
const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(500);
const JOIN_POLL: Duration = Duration::from_millis(2);
/// Bytes written per ready poll: one USB packet, well inside the pty's buffer.
const CHUNK: usize = 64;
/// Room for `struct termios` (72 bytes on arm64 and x86_64 macOS) with margin; only passed between
/// `tcgetattr`, `cfmakeraw` and `tcsetattr`, never read here.
const TERMIOS_BYTES: usize = 128;

/// Opaque storage for `struct termios`, aligned like its `unsigned long` fields (8 bytes) because
/// the C calls write through it.
#[repr(C, align(8))]
struct Termios([u8; TERMIOS_BYTES]);

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

unsafe extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    /// macOS: writes the slave's path into `buf` (NUL-terminated); 0 or an error number.
    fn ptsname_r(fd: i32, buf: *mut u8, len: usize) -> i32;
    fn tcgetattr(fd: i32, termios: *mut u8) -> i32;
    fn cfmakeraw(termios: *mut u8);
    fn tcsetattr(fd: i32, action: i32, termios: *const u8) -> i32;
    fn poll(fds: *mut PollFd, nfds: u32, timeout: i32) -> i32;
    fn ioctl(fd: i32, request: u64, ...) -> i32;
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

/// Opens a pty master a spawned child does not inherit: `O_CLOEXEC` at open, or `FD_CLOEXEC`
/// right after where the host refuses the flag with `EINVAL`.
fn open_master() -> io::Result<File> {
    // SAFETY: plain POSIX call; the descriptor it returns is owned below.
    let mut fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY | O_CLOEXEC) };
    if fd < 0 && io::Error::last_os_error().raw_os_error() == Some(EINVAL) {
        // SAFETY: as above.
        fd = unsafe { posix_openpt(O_RDWR | O_NOCTTY) };
        if fd >= 0 {
            // SAFETY: `fd` is open; `F_SETFD` takes one `int` argument.
            if unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) } != 0 {
                let e = io::Error::last_os_error();
                // SAFETY: `fd` is a fresh, open descriptor nothing else owns.
                drop(unsafe { File::from_raw_fd(fd) });
                return Err(e);
            }
        }
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, open descriptor; `File` takes sole ownership and closes it.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn ready(fd: RawFd, events: i16) -> io::Result<bool> {
    let mut p = PollFd {
        fd,
        events,
        revents: 0,
    };
    // SAFETY: one valid `pollfd` for the duration of the call.
    let n = unsafe { poll(&mut p, 1, POLL_MS) };
    match n {
        -1 => {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(e)
            }
        }
        0 => Ok(false),
        _ => Ok(p.revents & events != 0),
    }
}

#[derive(Debug)]
pub struct PtyEndpoint {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    /// The slave kept open by the endpoint, so the master never sees a hang-up between clients
    /// and unread output waits in the slave's queue.
    _slave: File,
}

impl PtyEndpoint {
    pub fn open(link: Arc<UsjLink>) -> io::Result<PtyEndpoint> {
        let master = open_master()?;
        let fd = master.as_raw_fd();
        // SAFETY: `fd` is the open master.
        if unsafe { grantpt(fd) } != 0 || unsafe { unlockpt(fd) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut name = [0u8; 128];
        // SAFETY: `name` is writable for its whole length, which is what is passed.
        let rc = unsafe { ptsname_r(fd, name.as_mut_ptr(), name.len()) };
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        let path = CStr::from_bytes_until_nul(&name)
            .map_err(|_| io::Error::other("ptsname_r returned no NUL"))?
            .to_str()
            .map_err(|_| io::Error::other("the pty path is not UTF-8"))?
            .to_owned();
        let path = PathBuf::from(path);
        let slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(&path)?;
        make_raw(slave.as_raw_fd())?;

        let tap = link.attach();
        let stop = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        let reader = master.try_clone()?;
        threads.push(
            std::thread::Builder::new()
                .name("usj-pty-in".to_owned())
                .spawn({
                    let link = Arc::clone(&link);
                    let stop = Arc::clone(&stop);
                    move || {
                        crate::platform::machine_thread();
                        pump_in(reader, &link, &stop)
                    }
                })?,
        );
        threads.push(
            std::thread::Builder::new()
                .name("usj-pty-out".to_owned())
                .spawn({
                    let stop = Arc::clone(&stop);
                    move || {
                        crate::platform::machine_thread();
                        pump_out(master, &link, &tap, &stop);
                        drop(tap);
                    }
                })?,
        );
        Ok(PtyEndpoint {
            path,
            stop,
            threads,
            _slave: slave,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn close(mut self) -> Shutdown {
        self.shutdown()
    }

    /// Sets the stop flag and joins each pump until [`SHUTDOWN_DEADLINE`], abandoning any still
    /// running. An abandoned thread exits soon after: closing the slave ends its blocking write,
    /// and it then sees the stop flag.
    fn shutdown(&mut self) -> Shutdown {
        self.stop.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + SHUTDOWN_DEADLINE;
        let mut report = Shutdown { abandoned: 0 };
        for t in self.threads.drain(..) {
            if !join_until(t, deadline) {
                report.abandoned += 1;
            }
        }
        report
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shutdown {
    pub abandoned: usize,
}

impl Shutdown {
    pub fn note(self) -> Option<String> {
        (self.abandoned > 0).then(|| {
            format!(
                "pty endpoint: {} of its pump threads did not stop within {} ms and {} abandoned; \
                 a slave nobody reads blocks the write pump",
                self.abandoned,
                SHUTDOWN_DEADLINE.as_millis(),
                if self.abandoned == 1 { "was" } else { "were" },
            )
        })
    }
}

fn join_until(t: JoinHandle<()>, deadline: Instant) -> bool {
    loop {
        if t.is_finished() {
            let _ = t.join();
            return true;
        }
        if Instant::now() >= deadline {
            drop(t);
            return false;
        }
        std::thread::sleep(JOIN_POLL);
    }
}

impl Drop for PtyEndpoint {
    /// Bounded, so a drop during a panic with bytes queued lets the panic be the failure. The note
    /// goes to stderr because a drop has nowhere else; [`PtyEndpoint::close`] returns it instead.
    fn drop(&mut self) {
        if let Some(note) = self.shutdown().note() {
            eprintln!("{note}");
        }
    }
}

fn make_raw(fd: RawFd) -> io::Result<()> {
    let mut termios = Termios([0u8; TERMIOS_BYTES]);
    let ptr = std::ptr::addr_of_mut!(termios.0).cast::<u8>();
    // SAFETY: `termios` is larger than `struct termios` and aligned like it, and `fd` is an open
    // terminal.
    unsafe {
        if tcgetattr(fd, ptr) != 0 {
            return Err(io::Error::last_os_error());
        }
        cfmakeraw(ptr);
        if tcsetattr(fd, TCSANOW, ptr) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn pump_in(mut master: File, link: &UsjLink, stop: &AtomicBool) {
    let mut buf = [0u8; 1024];
    while !stop.load(Ordering::SeqCst) {
        match ready(master.as_raw_fd(), POLLIN) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        match master.read(&mut buf) {
            Ok(0) => std::thread::sleep(Duration::from_millis(POLL_MS as u64)),
            Ok(n) => {
                if !link.send_bytes(&buf[..n]) {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // EIO while no slave is open; the endpoint's own slave makes it rare.
            Err(_) => std::thread::sleep(Duration::from_millis(POLL_MS as u64)),
        }
    }
}

fn pump_out(mut master: File, link: &UsjLink, tap: &Tap, stop: &AtomicBool) {
    let mut pending: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    while !stop.load(Ordering::SeqCst) {
        if pending.is_empty() {
            match link.read_output(tap, &mut buf, Duration::from_millis(POLL_MS as u64)) {
                None => return,
                Some(n) => pending.extend_from_slice(&buf[..n]),
            }
            continue;
        }
        match ready(master.as_raw_fd(), POLLOUT) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(_) => return,
        }
        let n = pending.len().min(CHUNK);
        match master.write(&pending[..n]) {
            Ok(written) => {
                pending.drain(..written);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(Duration::from_millis(POLL_MS as u64)),
        }
    }
}

/// Whether the terminal at `path` answers `TIOCMGET`. A macOS pty gives `Ok(false)` ([`ENOTTY`]).
pub fn modem_lines_supported(path: &Path) -> io::Result<bool> {
    let tty = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(O_NOCTTY)
        .open(path)?;
    let mut bits: i32 = 0;
    // SAFETY: `TIOCMGET` writes one `int` through the pointer, which is valid for the call.
    let rc = unsafe { ioctl(tty.as_raw_fd(), TIOCMGET, &mut bits as *mut i32) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(ENOTTY) => Ok(false),
        _ => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::super::live::echo::EchoMachine;
    use super::super::live::{LiveRunner, Pacing};

    #[test]
    fn bytes_cross_the_pty_both_ways_and_it_has_no_modem_lines() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = PtyEndpoint::open(runner.link()).expect("a pty pair");
        assert!(ep.path().starts_with("/dev/"), "{}", ep.path().display());
        let mut client = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(ep.path())
            .expect("the slave opens");
        client.write_all(b"ping").expect("write");
        let mut got = Vec::new();
        let mut buf = [0u8; 16];
        while got.len() < 4 {
            assert!(ready(client.as_raw_fd(), POLLIN).is_ok());
            if let Ok(n) = client.read(&mut buf) {
                got.extend_from_slice(&buf[..n]);
            }
        }
        assert_eq!(got, b"PING");
        assert!(!modem_lines_supported(ep.path()).expect("the query runs"));
        assert!(LIMITATION.contains("rfc2217://"));
        ep.close();
        runner.stop();
    }

    #[test]
    fn the_termios_buffer_is_aligned_like_the_struct() {
        assert_eq!(std::mem::align_of::<Termios>(), 8);
        assert!(std::mem::size_of::<Termios>() >= 72);
    }

    #[test]
    fn the_master_is_close_on_exec() {
        let master = open_master().expect("a pty master");
        // SAFETY: `F_GETFD` on an open descriptor takes no argument.
        let flags = unsafe { fcntl(master.as_raw_fd(), F_GETFD) };
        assert!(flags >= 0, "{}", io::Error::last_os_error());
        assert_eq!(flags & FD_CLOEXEC, FD_CLOEXEC);
    }

    fn read_pty(client: &mut File, want: usize) -> Vec<u8> {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut got = Vec::new();
        let mut buf = [0u8; 64];
        while got.len() < want {
            assert!(
                std::time::Instant::now() < deadline,
                "the pty stayed silent"
            );
            if ready(client.as_raw_fd(), POLLIN).expect("poll")
                && let Ok(n) = client.read(&mut buf)
            {
                got.extend_from_slice(&buf[..n]);
            }
        }
        got
    }

    /// A `PURGE-DATA` from the TCP client clears that client's queue and nothing else's; clearing
    /// every tap once lost the pty's boot log during a flash over `rfc2217://`. Checked with a spy
    /// subscription and end to end on the pty, whose ~1 KB buffer leaves most of [`FLOOD`] in the
    /// link at the purge.
    #[test]
    fn a_tcp_purge_leaves_the_pty_queue_intact() {
        use super::super::rfc2217::{
            IAC, OPT_BINARY, OPT_COM_PORT, SB, SE, SERVER_OFFSET, WILL, cmd, purge,
        };
        use super::super::tcp::{TcpEndpoint, TcpOptions};
        use std::net::TcpStream;

        const FLOOD: usize = 64 * 1024;

        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        let pty = PtyEndpoint::open(runner.link()).expect("a pty pair");
        let tcp = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        let mut slave = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(pty.path())
            .expect("the slave opens");
        let mut socket = TcpStream::connect(tcp.addr()).expect("connect");
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        // An `IAC` first byte selects RFC 2217; this option negotiation is otherwise uninteresting.
        socket.write_all(&[IAC, WILL, OPT_BINARY]).expect("write");
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while link.attached() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the TCP client never attached"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The spy: a third transport that never reads, so its queue is what the fan-out left it.
        let spy = link.attach();

        assert!(link.send_bytes(&vec![b'a'; FLOOD]));
        // Wait until the TCP client has the whole echo: the runner fans a slice out to every tap
        // in one pass, so the spy and the pty then hold the same bytes.
        let mut seen: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        fn read_socket(socket: &mut TcpStream, seen: &mut Vec<u8>, buf: &mut [u8; 4096]) {
            let n = socket.read(buf).expect("the client stream reads");
            assert!(n > 0, "the server closed the connection");
            seen.extend_from_slice(&buf[..n]);
        }
        let mut echoed = 0usize;
        while echoed < FLOOD {
            assert!(
                std::time::Instant::now() < deadline,
                "the TCP client is short {} of {FLOOD} echoed bytes",
                FLOOD - echoed
            );
            seen.clear();
            read_socket(&mut socket, &mut seen, &mut buf);
            echoed += seen.iter().filter(|b| **b == b'A').count();
        }
        assert_eq!(echoed, FLOOD);

        socket
            .write_all(&[
                IAC,
                SB,
                OPT_COM_PORT,
                cmd::PURGE_DATA,
                purge::RECEIVE,
                IAC,
                SE,
            ])
            .expect("write the purge");
        // The ack is written after the events are applied, so reading it is a barrier. Guest data
        // is all `A`, so the ack cannot be confused with it.
        let ack = [
            IAC,
            SB,
            OPT_COM_PORT,
            cmd::PURGE_DATA + SERVER_OFFSET,
            purge::RECEIVE,
            IAC,
            SE,
        ];
        seen.clear();
        while !seen.windows(ack.len()).any(|w| w == ack) {
            assert!(
                std::time::Instant::now() < deadline,
                "the server never acknowledged the purge"
            );
            read_socket(&mut socket, &mut seen, &mut buf);
            if seen.len() > 4096 {
                seen.drain(..seen.len() - ack.len());
            }
        }

        // Nothing is being added to either queue any more, so both counts are exact.
        let mut kept = 0usize;
        loop {
            let n = link
                .read_output(&spy, &mut buf, Duration::from_millis(200))
                .expect("the runner is live");
            if n == 0 {
                break;
            }
            kept += n;
        }

        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut got = 0usize;
        let mut wrong = 0usize;
        let mut pty_buf = [0u8; 4096];
        while got < FLOOD && std::time::Instant::now() < deadline {
            if ready(slave.as_raw_fd(), POLLIN).expect("poll")
                && let Ok(n) = slave.read(&mut pty_buf)
            {
                wrong += pty_buf[..n].iter().filter(|b| **b != b'A').count();
                got += n;
            }
        }

        // Close every endpoint before asserting, so a failure reports the difference instead of
        // hanging on full buffers.
        drop(socket);
        tcp.close();
        pty.close();
        runner.stop();

        assert_eq!(
            kept,
            FLOOD,
            "the TCP client's purge took {} bytes from another transport's queue",
            FLOOD - kept
        );
        assert_eq!(wrong, 0, "the pty carried something other than the echo");
        assert_eq!(
            got,
            FLOOD,
            "the pty is short {} of {FLOOD} bytes after the TCP client's purge",
            FLOOD - got
        );
    }

    #[test]
    fn a_tcp_client_leaving_does_not_detach_the_pty() {
        use super::super::tcp::{TcpEndpoint, TcpOptions};
        use std::net::TcpStream;

        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let link = runner.link();
        let pty = PtyEndpoint::open(runner.link()).expect("a pty pair");
        let tcp = TcpEndpoint::bind(runner.link(), TcpOptions::default()).expect("bind");
        let mut client = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(pty.path())
            .expect("the slave opens");
        let mut socket = TcpStream::connect(tcp.addr()).expect("connect");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while link.attached() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "the TCP client never attached"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(super::super::detect::SILENCE + Duration::from_millis(100));
        socket.write_all(b"ab").expect("write");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let mut two = [0u8; 2];
        socket
            .read_exact(&mut two)
            .expect("the TCP client gets the echo");
        assert_eq!(&two, b"AB");
        assert_eq!(read_pty(&mut client, 2), b"AB", "the pty gets it too");

        drop(socket);
        while link.attached() > 1 {
            assert!(std::time::Instant::now() < deadline + Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        client.write_all(b"cd").expect("write");
        assert_eq!(read_pty(&mut client, 2), b"CD", "the pty still receives");
        tcp.close();
        pty.close();
        runner.stop();
    }

    /// Fills the slave queue with nobody reading, which blocks the write pump's master write.
    fn wedge_with_queued_bytes(link: &UsjLink) {
        const FLOOD: usize = 64 * 1024;
        assert!(link.send_bytes(&vec![b'a'; FLOOD]));
        std::thread::sleep(Duration::from_millis(10 * POLL_MS as u64));
    }

    /// Without the deadline this test hung instead of failing: the drop joined a pump blocked on a
    /// full slave queue. Checked against wall time, since the bound is what is under test.
    #[test]
    fn dropping_a_pty_with_a_full_queue_and_no_reader_returns_within_the_deadline() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = PtyEndpoint::open(runner.link()).expect("a pty pair");
        wedge_with_queued_bytes(&runner.link());
        let started = Instant::now();
        drop(ep);
        let took = started.elapsed();
        runner.stop();
        assert!(
            took < SHUTDOWN_DEADLINE + JOIN_POLL + Duration::from_secs(2),
            "the drop took {took:?}, past the {SHUTDOWN_DEADLINE:?} deadline"
        );
    }

    /// Through `close`, the report says a thread was abandoned rather than passing it off as a
    /// clean shutdown.
    #[test]
    fn a_shutdown_that_abandons_a_pump_reports_it() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = PtyEndpoint::open(runner.link()).expect("a pty pair");
        wedge_with_queued_bytes(&runner.link());
        let report = ep.close();
        runner.stop();
        assert_eq!(
            report.abandoned, 1,
            "the write pump is blocked on a full slave queue, the read pump is not"
        );
        let note = report.note().expect("an abandoned pump has a note");
        assert!(note.contains("did not stop"), "{note}");
    }

    #[test]
    fn an_unwedged_shutdown_reports_nothing_abandoned() {
        let runner = LiveRunner::start(EchoMachine::new(), Pacing::Max)
            .map_err(|(_, e)| e)
            .expect("spawn");
        let ep = PtyEndpoint::open(runner.link()).expect("a pty pair");
        let report = ep.close();
        runner.stop();
        assert_eq!(report, Shutdown::default());
        assert!(report.note().is_none());
    }
}
