//! Which core class a native measurement ran on, and the QoS class the measuring thread asked for.
//!
//! On an Apple M3 Pro (6 performance and 6 efficiency cores, `hw.perflevel0` and
//! `hw.perflevel1`) the same F6 run reads a busy speed S of about 275 MIPS on a performance core
//! and 52 to 123 on an efficiency core, and a run the scheduler moves between the two reads
//! whatever mix it got. Over 100 single runs, S against the share of the S phase's CPU time on the
//! performance cluster fits `S = 88 + 180 x share` for F6 and `S = 64 + 209 x share` for F5
//! (r = 0.97 both). A low S on a shared host is placement, not code.
//!
//! **The QoS class is neither the cause nor the cure.** A process started from a terminal or an
//! agent harness already inherits `QOS_CLASS_USER_INTERACTIVE`, and so do competing builds; when
//! more threads want the performance cluster than it has cores, the scheduler spreads them over
//! both clusters whatever their class, well below a load average of one thread per core.
//! [`request_interactive`] is still called so the class a record carries is the same under any
//! launcher (`taskpolicy -c utility` starts a process at `utility`). It cannot undo a
//! darwin-background clamp (`taskpolicy -b`): the class reads back user-interactive while the run
//! gets no performance-cluster time, which only the reading below sees.
//!
//! What fixes the measurement is observing the placement: [`CoreTime`] reads the process's CPU
//! time on the performance cluster around every window, and a run is classified by the share of
//! its measured windows, and of the windows S came from, that ran there ([`classify`]).

/// Least share of a phase's CPU time on the performance cluster for the phase to count as run
/// there.
///
/// A run whose time is a share `p` on performance cores reads `S = p x Sp + (1 - p) x Se`, so its
/// S is low by `(1 - p) x (1 - Se / Sp)`. With `Se / Sp` about 0.35 on an M3 Pro (F6: 95 against
/// 275), 95 % bounds that at 3.3 %, a third of the 10 % trend band, which leaves the band for the
/// code. UNVERIFIED: a design choice.
pub(crate) const RESIDENT_SHARE: f64 = 0.95;

/// Host CPU time of this process, in the OS's own ticks: all of it, and the part spent on the
/// performance cluster. Only differences and ratios are used, so the unit never matters. On macOS
/// it is `proc_pid_rusage` `RUSAGE_INFO_V6`, whose `ri_user_ptime` and `ri_system_ptime` include
/// the running thread's time so far, so a 3 ms window reads 3.000 ms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CoreTime {
    pub(crate) all: u64,
    pub(crate) perf: u64,
}

impl CoreTime {
    /// This process's CPU time so far, or `None` where the host does not report it by core class;
    /// the bench measures on one thread, so the process's time over a window is that thread's.
    pub(crate) fn now() -> Option<CoreTime> {
        CoreTime::of_process(std::process::id())
    }

    /// The CPU time so far of `pid`, one this user may inspect (`xtask bench-k` reads its spike
    /// child this way), or `None` where the host does not say.
    pub(crate) fn of_process(pid: u32) -> Option<CoreTime> {
        imp::core_time(pid)
    }

    /// The CPU time spent between `earlier` and `self`.
    pub(crate) fn since(self, earlier: CoreTime) -> CoreTime {
        CoreTime {
            all: self.all.saturating_sub(earlier.all),
            perf: self.perf.saturating_sub(earlier.perf),
        }
    }
}

/// Share of the CPU time of `times` that ran on the performance cluster, or `None` when any window
/// has no reading or none of them used any CPU time.
pub(crate) fn perf_share(times: impl IntoIterator<Item = Option<CoreTime>>) -> Option<f64> {
    let mut all = 0u64;
    let mut perf = 0u64;
    for t in times {
        let t = t?;
        all += t.all;
        perf += t.perf.min(t.all);
    }
    (all > 0).then(|| perf as f64 / all as f64)
}

/// Where a run's measured phases ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cluster {
    /// Every observed phase ran at least [`RESIDENT_SHARE`] on the performance cluster.
    Performance,
    /// Every observed phase ran at least [`RESIDENT_SHARE`] on the efficiency cluster: a launcher
    /// clamped the process there (`taskpolicy -b` does, and the thread's QoS class still reads
    /// user-interactive), so its host time is that of cores no native target is set for.
    Efficiency,
    /// Anything between: its host time is a mix of two core speeds.
    Mixed,
    /// The host has one core class, so there is nothing to be resident on.
    OneClass,
    /// The host has several classes and reports no split (Windows, an older macOS), or has an
    /// unknown number of them.
    Unobserved,
}

impl Cluster {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Cluster::Performance => "performance",
            Cluster::Efficiency => "efficiency",
            Cluster::Mixed => "mixed",
            Cluster::OneClass => "one-class",
            Cluster::Unobserved => "unobserved",
        }
    }

    /// Whether a run on this cluster measured the cores the native targets are for, so its
    /// host time may be gated and may baseline another run: the performance cluster, or every
    /// core of a host that has one class or does not say which it used.
    pub(crate) fn measures(self) -> bool {
        matches!(
            self,
            Cluster::Performance | Cluster::OneClass | Cluster::Unobserved
        )
    }
}

/// Classifies a run from the performance-cluster shares of its phases on this host.
pub(crate) fn classify(shares: &[Option<f64>]) -> Cluster {
    classify_on(core_classes(), shares)
}

/// [`classify`] on a host with `classes` core classes (`None`: unknown). A phase with no share
/// (a phase that did not run, or an S that was not resolvable) is left out; a run with no observed
/// phase at all is unobserved.
pub(crate) fn classify_on(classes: Option<u32>, shares: &[Option<f64>]) -> Cluster {
    match classes {
        Some(1) => return Cluster::OneClass,
        None | Some(0) => return Cluster::Unobserved,
        Some(_) => {}
    }
    let observed: Vec<f64> = shares.iter().flatten().copied().collect();
    if observed.is_empty() {
        Cluster::Unobserved
    } else if observed.iter().all(|s| *s >= RESIDENT_SHARE) {
        Cluster::Performance
    } else if observed.iter().all(|s| *s <= 1.0 - RESIDENT_SHARE) {
        Cluster::Efficiency
    } else {
        Cluster::Mixed
    }
}

/// Core classes of this host: `sysctl -n hw.nperflevels` on macOS (2 on Apple Silicon, 1 on an
/// Intel Mac), `None` where the host does not say. Read once per process.
pub(crate) fn core_classes() -> Option<u32> {
    static CLASSES: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CLASSES.get_or_init(imp::core_classes)
}

/// Asks for the user-interactive QoS class on the calling thread, as every product machine thread
/// does, and returns the class it then runs at (`pemu_host::platform::machine_thread`).
pub(crate) fn request_interactive() -> &'static str {
    pemu_host::platform::machine_thread()
}

/// The QoS class the calling thread runs at, by name, read back through the platform layer.
pub(crate) fn qos_now() -> &'static str {
    pemu_host::platform::thread_qos_name()
}

#[cfg(target_os = "macos")]
mod imp {
    use super::CoreTime;

    /// `RUSAGE_INFO_V6` (`sys/resource.h`).
    const RUSAGE_INFO_V6: i32 = 6;

    /// `struct rusage_info_v6` (`sys/resource.h`): a 16-byte UUID and 56 `uint64_t` fields, 464
    /// bytes in all. Only the four fields below are read; the rest are kept as an array so the
    /// kernel has the whole struct to write.
    #[repr(C)]
    struct RusageInfoV6 {
        uuid: [u8; 16],
        fields: [u64; 56],
    }

    /// Indices into [`RusageInfoV6::fields`]: `ri_user_time` and `ri_system_time` at byte
    /// offsets 16 and 24, `ri_user_ptime` and `ri_system_ptime` at 304 and 312 (`offsetof` on the
    /// macOS 26 SDK), each less the UUID's 16 and over 8.
    const USER_TIME: usize = 0;
    const SYSTEM_TIME: usize = 1;
    const USER_PTIME: usize = 36;
    const SYSTEM_PTIME: usize = 37;

    const _: () = assert!(std::mem::size_of::<RusageInfoV6>() == 464);

    // Process spawning and ownership stay in `pemu-host`; this only reads usage through
    // libSystem, which std already links, in a tool that is never shipped.
    unsafe extern "C" {
        /// `libproc.h`: fills `buffer` with the `flavor` resource usage of `pid`; 0 on success.
        fn proc_pid_rusage(pid: i32, flavor: i32, buffer: *mut RusageInfoV6) -> i32;
    }

    pub(super) fn core_time(pid: u32) -> Option<CoreTime> {
        let mut info = RusageInfoV6 {
            uuid: [0; 16],
            fields: [0; 56],
        };
        let pid = i32::try_from(pid).ok()?;
        // SAFETY: `info` is a live, writable `rusage_info_v6` of the size the V6 flavor writes
        // (asserted above), and the call writes nothing else.
        let status = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V6, &mut info) };
        if status != 0 {
            return None;
        }
        let f = &info.fields;
        Some(CoreTime {
            all: f[USER_TIME] + f[SYSTEM_TIME],
            perf: f[USER_PTIME] + f[SYSTEM_PTIME],
        })
    }

    pub(super) fn core_classes() -> Option<u32> {
        let out = std::process::Command::new("/usr/sbin/sysctl")
            .args(["-n", "hw.nperflevels"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::CoreTime;

    pub(super) fn core_time(_pid: u32) -> Option<CoreTime> {
        None
    }

    pub(super) fn core_classes() -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(all: u64, perf: u64) -> Option<CoreTime> {
        Some(CoreTime { all, perf })
    }

    #[test]
    fn the_share_is_of_cpu_time_and_any_missing_window_makes_it_unknown() {
        assert_eq!(perf_share([t(100, 100), t(100, 90)]), Some(0.95));
        assert_eq!(perf_share([t(100, 100), None]), None);
        assert_eq!(perf_share([t(0, 0)]), None, "no CPU time is no share");
        assert_eq!(perf_share(std::iter::empty()), None);
        // A reading that claims more performance time than time is capped at all of it.
        assert_eq!(perf_share([t(10, 20)]), Some(1.0));
        let later = CoreTime { all: 30, perf: 25 };
        assert_eq!(
            later.since(CoreTime { all: 10, perf: 5 }),
            CoreTime { all: 20, perf: 20 }
        );
        assert_eq!(CoreTime::default().since(later), CoreTime::default());
    }

    #[test]
    fn a_run_is_performance_only_when_every_observed_phase_is_resident() {
        let two = Some(2);
        assert_eq!(
            classify_on(two, &[Some(1.0), Some(0.95)]),
            Cluster::Performance
        );
        assert_eq!(classify_on(two, &[Some(1.0), Some(0.949)]), Cluster::Mixed);
        assert_eq!(classify_on(two, &[Some(0.5), None]), Cluster::Mixed);
        assert_eq!(
            classify_on(two, &[Some(0.0), Some(0.05)]),
            Cluster::Efficiency
        );
        assert_eq!(classify_on(two, &[Some(0.0), Some(0.051)]), Cluster::Mixed);
        assert_eq!(classify_on(two, &[Some(0.0), Some(1.0)]), Cluster::Mixed);
        let measures: Vec<Cluster> = [
            Cluster::Performance,
            Cluster::Efficiency,
            Cluster::Mixed,
            Cluster::OneClass,
            Cluster::Unobserved,
        ]
        .into_iter()
        .filter(|c| c.measures())
        .collect();
        assert_eq!(
            measures,
            [Cluster::Performance, Cluster::OneClass, Cluster::Unobserved]
        );
        assert_eq!(classify_on(two, &[None, Some(0.99)]), Cluster::Performance);
        assert_eq!(classify_on(two, &[None, None]), Cluster::Unobserved);
        assert_eq!(classify_on(Some(1), &[Some(0.0)]), Cluster::OneClass);
        assert_eq!(classify_on(None, &[Some(1.0)]), Cluster::Unobserved);
        assert_eq!(classify_on(Some(0), &[Some(1.0)]), Cluster::Unobserved);
    }

    /// The reader on the host itself: a spin of about 3 ms reads as CPU time, and on a host that
    /// splits it by class the performance part is never more than the whole.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_host_reports_this_threads_time_as_it_runs() {
        let before = CoreTime::now().expect("macOS reports rusage_info_v6");
        let start = std::time::Instant::now();
        let mut x = 0u64;
        while start.elapsed() < std::time::Duration::from_millis(3) {
            x = std::hint::black_box(x.wrapping_add(1));
        }
        let spent = CoreTime::now().expect("a second reading").since(before);
        assert!(
            spent.all > 0,
            "3 ms of spinning read as no CPU time: {spent:?}"
        );
        assert!(spent.perf <= spent.all, "{spent:?}");
        assert_ne!(qos_now(), "unknown");
    }
}
