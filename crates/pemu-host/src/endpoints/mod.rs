//! The USB Serial/JTAG host endpoints: the protocol pieces ([`rfc2217`], [`detect`], [`slip`]),
//! the live runner every endpoint shares ([`live`]), the transports ([`tcp`], and `pty` on macOS),
//! and [`HostEndpoints`], the `EndpointHost` the daemon and CLI install with [`install`].

pub mod detect;
// The external HCI transport: a loopback listener carrying an H4 stream instead of a console.
pub mod hci;
pub mod live;
// A pty is macOS-only: on Windows the endpoint is reached over TCP, and `rfc2217://` covers tools
// that want a port.
#[cfg(target_os = "macos")]
pub mod pty;
pub mod rfc2217;
pub mod slip;
pub mod tcp;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use pemu_api::commands::endpoint::{
    Closed, EndpointArgs, EndpointClock, EndpointHost, EndpointInfo, OpenFailed, Opened, set_host,
};
use pemu_api::commands::start::Session;
use pemu_api::error::{ApiError, E_INTERNAL};
use pemu_api::instance::InstanceId;

use live::{AgentLinked, AgentSlot, LiveRunner, Pacing, UsjLink};
use tcp::{TcpEndpoint, TcpOptions};

/// Who drives an open instance: the live runner holding its session, or the agent (the session
/// stays in the pool, wrapped by [`AgentLinked`]).
enum Driver {
    Live(LiveRunner<Session>),
    Agent {
        slot: Arc<AgentSlot>,
        link: Arc<UsjLink>,
    },
}

impl Driver {
    fn link(&self) -> Arc<UsjLink> {
        match self {
            Driver::Live(runner) => runner.link(),
            Driver::Agent { link, .. } => Arc::clone(link),
        }
    }
}

struct Open {
    tcp: Option<TcpEndpoint>,
    #[cfg(target_os = "macos")]
    pty: Option<pty::PtyEndpoint>,
    auto_download: bool,
    driver: Driver,
}

impl Open {
    fn info(&self) -> EndpointInfo {
        let link = self.driver.link();
        let status = self.tcp.as_ref().map(TcpEndpoint::status);
        #[cfg(target_os = "macos")]
        let (pty_path, pty_limitation) = match &self.pty {
            Some(p) => (
                Some(p.path().display().to_string()),
                Some(pty::LIMITATION.to_owned()),
            ),
            None => (None, None),
        };
        #[cfg(not(target_os = "macos"))]
        let (pty_path, pty_limitation) = (None, None);
        EndpointInfo {
            tcp_port: self.tcp.as_ref().map(TcpEndpoint::port),
            pty_path,
            pty_limitation,
            auto_download: self.auto_download,
            clock: match self.driver {
                Driver::Live(_) => EndpointClock::Endpoint,
                Driver::Agent { .. } => EndpointClock::Agent,
            },
            clients: link.attached(),
            connections: status.as_ref().map_or(0, |s| s.connections),
            last_mode: status
                .and_then(|s| s.last_mode)
                .map(|m| m.as_str().to_owned()),
            vt_us: link.vt().0 / 1_000_000,
            qos: link.qos(),
            reanchors: link.reanchors(),
        }
    }

    /// Closes the transports first, then stops the runner and takes the session back.
    fn close(self) -> Closed {
        let Open {
            tcp,
            #[cfg(target_os = "macos")]
            pty,
            driver,
            ..
        } = self;
        if let Some(t) = tcp {
            t.close();
        }
        #[cfg(target_os = "macos")]
        if let Some(p) = pty {
            p.close();
        }
        match driver {
            Driver::Live(runner) => Closed {
                clock: EndpointClock::Endpoint,
                session: runner.stop(),
            },
            Driver::Agent { slot, .. } => {
                slot.disconnect();
                Closed {
                    clock: EndpointClock::Agent,
                    session: None,
                }
            }
        }
    }
}

#[derive(Default)]
pub struct HostEndpoints {
    open: Mutex<BTreeMap<InstanceId, Open>>,
    /// The `--clock agent` wrapper of every instance that ever had one, so reopening reuses it
    /// instead of stacking a second.
    agent_slots: Mutex<BTreeMap<InstanceId, Arc<AgentSlot>>>,
}

impl HostEndpoints {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<InstanceId, Open>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[allow(clippy::type_complexity)]
    fn transports(
        link: &Arc<UsjLink>,
        args: &EndpointArgs,
    ) -> Result<(Option<TcpEndpoint>, OptPty), ApiError> {
        let tcp =
            if args.tcp {
                let opts = TcpOptions {
                    auto_download: args.auto_download,
                };
                Some(TcpEndpoint::bind(Arc::clone(link), opts).map_err(|e| {
                    ApiError::new(E_INTERNAL, format!("binding 127.0.0.1 failed: {e}"))
                })?)
            } else {
                None
            };
        #[cfg(target_os = "macos")]
        let pty = if args.pty {
            match pty::PtyEndpoint::open(Arc::clone(link)) {
                Ok(p) => Some(p),
                Err(e) => {
                    drop(tcp);
                    return Err(ApiError::new(
                        E_INTERNAL,
                        format!("opening a pty failed: {e}"),
                    ));
                }
            }
        } else {
            None
        };
        #[cfg(not(target_os = "macos"))]
        let pty = ();
        Ok((tcp, pty))
    }
}

#[cfg(target_os = "macos")]
type OptPty = Option<pty::PtyEndpoint>;
#[cfg(not(target_os = "macos"))]
type OptPty = ();

impl EndpointHost for HostEndpoints {
    fn open(&self, session: Session, args: &EndpointArgs) -> Result<Opened, Box<OpenFailed>> {
        let id = session.id;
        let (driver, session) = match args.clock {
            EndpointClock::Endpoint => {
                // Wall time at 1x while a host tool is connected, idle otherwise.
                let runner =
                    LiveRunner::start(session, Pacing::Wall(1.0)).map_err(|(session, e)| {
                        OpenFailed {
                            session: Some(session),
                            error: ApiError::new(
                                E_INTERNAL,
                                format!("the endpoint thread did not start: {e}"),
                            ),
                        }
                    })?;
                (Driver::Live(runner), None)
            }
            EndpointClock::Agent => {
                let (slot, fresh) = {
                    let mut slots = self.agent_slots.lock().unwrap_or_else(|e| e.into_inner());
                    match slots.get(&id) {
                        Some(slot) => (Arc::clone(slot), false),
                        None => {
                            let slot = Arc::new(AgentSlot::default());
                            slots.insert(id, Arc::clone(&slot));
                            (slot, true)
                        }
                    }
                };
                let mut session = if fresh {
                    let slot = Arc::clone(&slot);
                    session.with_backend(move |inner| Box::new(AgentLinked::new(inner, slot)))
                } else {
                    session
                };
                let link = UsjLink::starting_at(session.machine());
                slot.connect(Arc::clone(&link));
                (Driver::Agent { slot, link }, Some(session))
            }
        };
        let (tcp, pty) = match Self::transports(&driver.link(), args) {
            Ok(t) => t,
            Err(error) => {
                let closed = Open {
                    tcp: None,
                    #[cfg(target_os = "macos")]
                    pty: None,
                    auto_download: false,
                    driver,
                }
                .close();
                return Err(Box::new(OpenFailed {
                    session: closed.session.or(session),
                    error,
                }));
            }
        };
        #[cfg(not(target_os = "macos"))]
        let () = pty;
        let open = Open {
            tcp,
            #[cfg(target_os = "macos")]
            pty,
            auto_download: args.auto_download,
            driver,
        };
        let info = open.info();
        self.lock().insert(id, open);
        Ok(Opened { info, session })
    }

    fn close(&self, id: InstanceId) -> Option<Closed> {
        let open = self.lock().remove(&id)?;
        Some(open.close())
    }

    fn describe(&self, id: InstanceId) -> Option<EndpointInfo> {
        self.lock().get(&id).map(Open::info)
    }
}

/// Installs the process's [`HostEndpoints`] as the pool's endpoint host. Idempotent: a second call
/// keeps the first host, so open endpoints are never orphaned.
pub fn install() -> Arc<HostEndpoints> {
    static HOST: std::sync::OnceLock<Arc<HostEndpoints>> = std::sync::OnceLock::new();
    let host = Arc::clone(HOST.get_or_init(|| Arc::new(HostEndpoints::default())));
    set_host(Some(Arc::clone(&host) as Arc<dyn EndpointHost>));
    host
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::{Read, Write};
    use std::time::{Duration, Instant};

    use pemu_api::commands::endpoint::endpoint_on;
    use pemu_api::commands::start::{Pool, StartArgs};
    use pemu_api::commands::status::{StatusArgs, status_on};
    use pemu_api::error::E_LEASE;
    use pemu_api::instance::Lifecycle;
    use pemu_core::time::VTime;
    use pemu_testkit::mock_machine::MockScript;

    /// A pool with one started instance and its own endpoint host, isolated from other tests.
    fn running_pool(
        machine: Box<dyn pemu_machine::SnapshotMachine + Send>,
    ) -> (Mutex<Pool>, InstanceId) {
        let mut pool = Pool::new();
        pool.set_endpoint_host(Some(Arc::new(HostEndpoints::default())));
        let id = pool.attach(&StartArgs::default(), machine);
        pool.table_mut()
            .get_mut(id)
            .expect("attached")
            .transition(Lifecycle::Paused, VTime(0))
            .expect("started");
        (Mutex::new(pool), id)
    }

    fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_endpoint_clock_runs_live_only_while_a_client_is_connected() {
        let (pool, id) = running_pool(Box::new(MockScript::new().build()));
        let host = pool
            .lock()
            .expect("pool")
            .endpoint_host()
            .expect("installed");
        let host_dyn = Arc::clone(&host);
        let open = EndpointArgs {
            instance: Some(id.to_string()),
            tcp: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host_dyn)), &open, &pool).expect("opens");
        let port = out.json["tcp"]["port"].as_u64().expect("a port") as u16;
        assert_eq!(out.json["clock"], "endpoint");

        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            host.describe(id).expect("open").vt_us,
            0,
            "idle without a client"
        );

        let client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("loopback");
        wait_for("the client to drive the clock", || {
            host.describe(id)
                .is_some_and(|i| i.clients == 1 && i.vt_us > 0)
        });
        let refused = pool
            .lock()
            .expect("pool")
            .bind(
                pemu_api::commands::run::SPEC_RUN.annotations,
                Some(&id.to_string()),
            )
            .expect_err("the endpoint holds the lease");
        assert_eq!(refused.code, E_LEASE, "{refused:?}");
        assert!(
            refused.message.contains("`endpoint`"),
            "{}",
            refused.message
        );

        let status = status_on(
            &mut pool.lock().expect("pool"),
            &StatusArgs {
                instance: Some(id.to_string()),
            },
        )
        .expect("status answers a live instance");
        let row = &status.json["instances"][0];
        assert_eq!(row["busy"], true);
        assert_eq!(row["endpoint"]["tcp"]["port"], u64::from(port));
        let live_qos = row["endpoint"]["qos"].clone();
        assert!(live_qos.is_string(), "{row}");
        assert!(row["endpoint"]["reanchors"].is_u64(), "{row}");
        assert!(
            status.text.contains(&format!("rfc2217://127.0.0.1:{port}")),
            "{}",
            status.text
        );

        drop(client);
        wait_for("the client to leave", || {
            host.describe(id).is_some_and(|i| i.clients == 0)
        });
        std::thread::sleep(Duration::from_millis(20));
        let parked = host.describe(id).expect("open").vt_us;
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(host.describe(id).expect("open").vt_us, parked, "idle again");

        let close = EndpointArgs {
            instance: Some(id.to_string()),
            close: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host_dyn)), &close, &pool).expect("closes");
        assert!(out.json["final_vt_us"].as_u64().expect("final vt") >= parked);
        assert_eq!(out.receipt.to_json()["host_qos"], live_qos);
        assert!(
            out.json["insns"].as_u64().expect("insns") > 0,
            "live slices count"
        );
        assert!(host.describe(id).is_none());
        let mut pool = pool.lock().expect("pool");
        assert!(!pool.is_busy(id));
        assert!(pool.session_mut(id).is_some_and(|s| s.now().0 > 0));
        let lease = &pool.table().get(id).expect("p1").lease;
        assert_eq!(lease.holder(VTime(0)), None, "close released the lease");
    }

    #[test]
    fn stop_closes_open_endpoints_and_reports_the_live_run() {
        let (pool, id) = running_pool(Box::new(MockScript::new().build()));
        let host = pool
            .lock()
            .expect("pool")
            .endpoint_host()
            .expect("installed");
        let open = EndpointArgs {
            instance: Some(id.to_string()),
            tcp: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &open, &pool).expect("opens");
        let port = out.json["tcp"]["port"].as_u64().expect("a port") as u16;
        let client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("loopback");
        wait_for("live slices", || {
            host.describe(id).is_some_and(|i| i.vt_us > 0)
        });

        let mut guard = pool.lock().expect("pool");
        let out = pemu_api::commands::stop::stop_on(
            &mut guard,
            id,
            &pemu_api::commands::stop::StopArgs {
                instance: None,
                keep_artifacts: true,
            },
        )
        .expect("stop ends an instance an endpoint was running");
        assert!(out.json["final_vt_us"].as_u64().expect("vt") > 0);
        assert!(out.json["insns"].as_u64().expect("insns") > 0);
        assert!(host.describe(id).is_none(), "the endpoint closed first");
        assert!(!guard.is_busy(id));
        drop(client);
    }

    /// `--clock agent`: the instance stays with the agent, a client's bytes are journaled as agent
    /// calls run it, and their output reaches the client.
    #[test]
    fn the_agent_clock_carries_client_bytes_through_agent_runs() {
        let (pool, id) = running_pool(Box::new(live::echo::EchoMachine::new()));
        let host = pool
            .lock()
            .expect("pool")
            .endpoint_host()
            .expect("installed");
        let open = EndpointArgs {
            instance: Some(id.to_string()),
            tcp: true,
            clock: EndpointClock::Agent,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &open, &pool).expect("opens");
        let port = out.json["tcp"]["port"].as_u64().expect("a port") as u16;
        assert!(out.json["qos"].is_null(), "no live thread: {}", out.json);
        assert!(
            !pool.lock().expect("pool").is_busy(id),
            "the agent keeps the instance"
        );

        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("loopback");
        std::thread::sleep(detect::SILENCE + Duration::from_millis(100));
        client.write_all(b"hi").expect("write");
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            pool.lock()
                .expect("pool")
                .session(id)
                .expect("in the pool")
                .now(),
            VTime(0),
            "nothing runs between agent calls"
        );
        client
            .set_read_timeout(Some(Duration::from_millis(50)))
            .expect("timeout");
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !got.windows(2).any(|w| w == b"HI") {
            assert!(Instant::now() < deadline, "no echo: {got:?}");
            {
                let mut pool = pool.lock().expect("pool");
                let session = pool.session_mut(id).expect("in the pool");
                let until = VTime(session.now().0 + VTime::from_ms(1).0);
                session.run_until(until);
            }
            let mut buf = [0u8; 64];
            if let Ok(n) = client.read(&mut buf) {
                got.extend_from_slice(&buf[..n]);
            }
        }
        let close = EndpointArgs {
            instance: Some(id.to_string()),
            close: true,
            ..EndpointArgs::default()
        };
        let out = endpoint_on(Some(Arc::clone(&host)), &close, &pool).expect("closes");
        assert!(out.json["final_vt_us"].as_u64().expect("vt") > 0);
        assert!(host.describe(id).is_none());
    }
}
