//! The Windows arms of [`crate::platform::connect_loopback`], which answers a closed port as
//! refused at once, and [`crate::platform::listen_loopback`], a listener no socket can bind over.
//!
//! **Connect.** Windows retransmits the SYN after a loopback reset, so a refused `127.0.0.1`
//! connect took 2089 ms (measured, Windows 11 26200) and a bounded wait saw a time-out instead of
//! a refusal. `SIO_TCP_INITIAL_RTO` with `TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS` turns the
//! retransmissions off and must be set before the connect (Microsoft Learn, "SIO_TCP_INITIAL_RTO
//! Control Code"); std offers no unconnected socket, so the socket is connected here and handed
//! to std.
//!
//! **Listen.** "All server applications must set SO_EXCLUSIVEADDRUSE", before the bind
//! (Microsoft Learn, "Using SO_REUSEADDR and SO_EXCLUSIVEADDRUSE"). std's bind sets neither
//! option, so the listener is built here too.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::windows::io::FromRawSocket as _;
use std::time::Duration;

use windows_sys::Win32::Networking::WinSock::{
    AF_INET, AF_INET6, FD_SET, IN_ADDR, IN_ADDR_0, IN6_ADDR, IN6_ADDR_0, INVALID_SOCKET,
    IPPROTO_TCP, SIO_TCP_INITIAL_RTO, SO_EXCLUSIVEADDRUSE, SOCK_STREAM, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_IN6_0, SOCKET, SOCKET_ERROR, SOL_SOCKET, TCP_INITIAL_RTO_DEFAULT_RTT,
    TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS, TCP_INITIAL_RTO_PARAMETERS, TIMEVAL,
    WSA_FLAG_NO_HANDLE_INHERIT, WSA_FLAG_OVERLAPPED, WSADATA, WSAEWOULDBLOCK, WSAGetLastError,
    WSAIoctl, WSASocketW, WSAStartup, bind, connect, listen, select, setsockopt,
};

const BACKLOG: i32 = 128;

/// Initializes Winsock once. `WSAStartup` is reference counted, so one beside std's is harmless.
fn startup() -> io::Result<()> {
    static STARTED: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    let status = *STARTED.get_or_init(|| {
        // SAFETY: an out parameter this function owns; version 2.2 is the one std asks for.
        let mut data: WSADATA = unsafe { std::mem::zeroed() };
        unsafe { WSAStartup(0x0202, &mut data) }
    });
    match status {
        0 => Ok(()),
        code => Err(io::Error::from_raw_os_error(code)),
    }
}

fn last_error() -> io::Error {
    // SAFETY: no arguments; reads this thread's Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn sockaddr(addr: &SocketAddr) -> (SockAddr, i32) {
    match addr {
        SocketAddr::V4(v4) => (
            SockAddr {
                v4: SOCKADDR_IN {
                    sin_family: AF_INET,
                    sin_port: v4.port().to_be(),
                    sin_addr: IN_ADDR {
                        S_un: IN_ADDR_0 {
                            S_addr: u32::from_ne_bytes(v4.ip().octets()),
                        },
                    },
                    sin_zero: [0; 8],
                },
            },
            size_of::<SOCKADDR_IN>() as i32,
        ),
        SocketAddr::V6(v6) => (
            SockAddr {
                v6: SOCKADDR_IN6 {
                    sin6_family: AF_INET6,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: IN6_ADDR {
                        u: IN6_ADDR_0 {
                            Byte: v6.ip().octets(),
                        },
                    },
                    Anonymous: SOCKADDR_IN6_0 {
                        sin6_scope_id: v6.scope_id(),
                    },
                },
            },
            size_of::<SOCKADDR_IN6>() as i32,
        ),
    }
}

#[repr(C)]
union SockAddr {
    v4: SOCKADDR_IN,
    v6: SOCKADDR_IN6,
}

/// A new TCP socket of `addr`'s family, created as std creates its own. The caller hands it to a
/// std type at once, so it is closed on every path.
fn tcp_socket(addr: &SocketAddr) -> io::Result<SOCKET> {
    startup()?;
    let family = match addr {
        SocketAddr::V4(_) => AF_INET,
        SocketAddr::V6(_) => AF_INET6,
    };
    // SAFETY: plain arguments; the flags are std's, and `WSA_FLAG_NO_HANDLE_INHERIT` keeps the
    // socket out of every child.
    let socket: SOCKET = unsafe {
        WSASocketW(
            i32::from(family),
            SOCK_STREAM,
            IPPROTO_TCP,
            std::ptr::null(),
            0,
            WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
        )
    };
    if socket == INVALID_SOCKET {
        return Err(last_error());
    }
    Ok(socket)
}

/// A listener with `SO_EXCLUSIVEADDRUSE` set before the bind and never `SO_REUSEADDR`, blocking.
pub(crate) fn listen_exclusive(addr: &SocketAddr) -> io::Result<TcpListener> {
    listen_with(addr, Some(SO_EXCLUSIVEADDRUSE))
}

fn listen_with(addr: &SocketAddr, option: Option<i32>) -> io::Result<TcpListener> {
    let socket = tcp_socket(addr)?;
    // SAFETY: a socket this function just created and owns alone; the listener closes it on
    // drop, on every path below.
    let listener = unsafe { TcpListener::from_raw_socket(socket as u64) };
    if let Some(option) = option {
        let on: i32 = 1;
        // SAFETY: a `BOOL`-sized value and its size, which both options take (Microsoft Learn,
        // "SOL_SOCKET socket options").
        let set = unsafe {
            setsockopt(
                socket,
                SOL_SOCKET,
                option,
                (&raw const on).cast(),
                size_of::<i32>() as i32,
            )
        };
        if set == SOCKET_ERROR {
            return Err(last_error());
        }
    }
    let (name, len) = sockaddr(addr);
    // SAFETY: `name` holds a socket address of the socket's family, and `len` is its size.
    if unsafe { bind(socket, (&raw const name).cast::<SOCKADDR>(), len) } == SOCKET_ERROR {
        return Err(last_error());
    }
    // SAFETY: a bound socket this function owns.
    if unsafe { listen(socket, BACKLOG) } == SOCKET_ERROR {
        return Err(last_error());
    }
    Ok(listener)
}

/// Connects within `timeout` with no SYN retransmission, so a reset is a refusal at once. The
/// stream comes back blocking.
pub(crate) fn connect_loopback(addr: &SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot connect with a zero timeout",
        ));
    }
    let socket = tcp_socket(addr)?;
    // SAFETY: a socket this function just created and owns alone; the stream closes it on drop,
    // on every path below.
    let stream = unsafe { TcpStream::from_raw_socket(socket as u64) };

    let rto = TCP_INITIAL_RTO_PARAMETERS {
        Rtt: TCP_INITIAL_RTO_DEFAULT_RTT as u16,
        // The header value is `(USHORT)-2` stored into a `UCHAR`: 0xFE.
        MaxSynRetransmissions: TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS as u8,
    };
    let mut returned = 0u32;
    // SAFETY: the input structure and its size match the control code; no output buffer, no
    // overlapped operation.
    let set = unsafe {
        WSAIoctl(
            socket,
            SIO_TCP_INITIAL_RTO,
            (&raw const rto).cast(),
            size_of::<TCP_INITIAL_RTO_PARAMETERS>() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if set == SOCKET_ERROR {
        return Err(last_error());
    }

    stream.set_nonblocking(true)?;
    let (name, len) = sockaddr(addr);
    // SAFETY: `name` holds a socket address of the family the socket was created with, and
    // `len` is its size.
    if unsafe { connect(socket, (&raw const name).cast::<SOCKADDR>(), len) } == SOCKET_ERROR {
        // SAFETY: no arguments.
        let code = unsafe { WSAGetLastError() };
        if code != WSAEWOULDBLOCK {
            return Err(io::Error::from_raw_os_error(code));
        }
        let mut writable = FD_SET {
            fd_count: 1,
            fd_array: [0; 64],
        };
        writable.fd_array[0] = socket;
        let mut failed = FD_SET {
            fd_count: 1,
            fd_array: [0; 64],
        };
        failed.fd_array[0] = socket;
        let wait = TIMEVAL {
            tv_sec: timeout.as_secs().min(i32::MAX as u64) as i32,
            tv_usec: timeout.subsec_micros() as i32,
        };
        // SAFETY: two one-socket sets and a time-out this function owns. A non-blocking connect
        // reports success in the write set and failure in the except set.
        let ready = unsafe {
            select(
                0,
                std::ptr::null_mut(),
                &mut writable,
                &mut failed,
                &raw const wait,
            )
        };
        match ready {
            SOCKET_ERROR => return Err(last_error()),
            0 => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "connection timed out",
                ));
            }
            _ => {}
        }
        if let Some(e) = stream.take_error()? {
            return Err(e);
        }
        if failed.fd_count != 0 {
            return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
        }
    }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Instant;

    #[test]
    fn a_closed_loopback_port_is_refused_at_once() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
            listener.local_addr().expect("its address").port()
        };
        let started = Instant::now();
        let e = connect_loopback(
            &SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(5),
        )
        .expect_err("nothing listens");
        let took = started.elapsed();
        assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused, "{e}");
        assert!(
            took < Duration::from_millis(400),
            "refused in {took:?}, not after the 2 s of SYN retransmission a plain connect waits"
        );
    }

    /// No second socket binds the daemon's address and port, however it asks, and a wildcard
    /// bind of the same port gets none of its connections.
    ///
    /// Measured with one user account: a bind of the same address is `WSAEADDRINUSE` plain and
    /// `WSAEACCES` with `SO_REUSEADDR`; a wildcard bind succeeds and is "bound to all interfaces
    /// except the specific address", as Microsoft Learn's same-account table says. A plain
    /// first bind gets the same row, so for a same-account socket the option changes nothing
    /// visible. A second account was not tried (UNVERIFIED).
    #[test]
    fn nothing_binds_over_the_daemons_listener_and_a_wildcard_bind_gets_none_of_its_connections() {
        use windows_sys::Win32::Networking::WinSock::SO_REUSEADDR;
        let first = listen_exclusive(&SocketAddr::from(([127, 0, 0, 1], 0))).expect("loopback");
        let addr = first.local_addr().expect("its address");
        let wildcard = SocketAddr::from(([0, 0, 0, 0], addr.port()));

        for (how, second, kind) in [
            ("plain", TcpListener::bind(addr), io::ErrorKind::AddrInUse),
            (
                "SO_REUSEADDR",
                listen_with(&addr, Some(SO_REUSEADDR)),
                io::ErrorKind::PermissionDenied,
            ),
        ] {
            let e = second.map(|_| ()).expect_err(how);
            assert_eq!(e.kind(), kind, "{how}, same address: {e}");
        }

        first.set_nonblocking(true).expect("nonblocking");
        for (how, option) in [("plain", None), ("SO_REUSEADDR", Some(SO_REUSEADDR))] {
            let other = listen_with(&wildcard, option)
                .unwrap_or_else(|e| panic!("{how}, wildcard: the documented success, got {e}"));
            other.set_nonblocking(true).expect("nonblocking");
            let _client = TcpStream::connect(addr).expect("connects");
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match first.accept() {
                    Ok(_) => break,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "{how}, wildcard: the daemon's listener never saw the connection"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            }
            let e = other.accept().map(|_| ()).expect_err("nothing reached it");
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{how}, wildcard: {e}");
        }
    }

    #[test]
    fn an_open_loopback_port_connects_and_carries_bytes() {
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(bind).expect("loopback");
            let addr = listener.local_addr().expect("its address");
            let server = std::thread::spawn(move || {
                let (mut peer, _) = listener.accept().expect("accept");
                let mut byte = [0u8; 1];
                peer.read_exact(&mut byte).expect("read");
                peer.write_all(&byte).expect("echo");
            });
            let mut stream = connect_loopback(&addr, Duration::from_secs(5)).expect("connects");
            stream.write_all(b"z").expect("write");
            let mut back = [0u8; 1];
            stream
                .read_exact(&mut back)
                .expect("a blocking read waits for the echo");
            assert_eq!(&back, b"z", "{bind}");
            server.join().expect("the server");
        }
    }
}
