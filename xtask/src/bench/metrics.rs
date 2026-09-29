//! The 100 ms windows of a run and the busy speed S and idle cost c derived from them.

use serde_json::{Value, json};

use super::cores::{self, Cluster, CoreTime};

/// One 100 ms virtual window of a run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Window {
    /// Instructions actually executed in the window: retired minus fast-forward credit.
    pub(super) busy_insns: u64,
    /// Virtual picoseconds the idle path skipped in the window.
    pub(super) idle_ps: u64,
    /// Virtual picoseconds the window spans (the last one of a run can be shorter).
    pub(super) span_ps: u64,
    /// Host nanoseconds the window took.
    pub(super) host_ns: u64,
    /// This process's CPU time in the window, all of it and the part on the performance cluster,
    /// or `None` where the host does not split it by core class (the `cores` module).
    pub(super) cores: Option<CoreTime>,
}

/// Fewest windows that did not idle before S is resolvable from them, unless they are the whole
/// phase: a workload that never idles measures S over all of itself however short it is, and F1
/// is 2 windows long. There is no "share of the run" rule: every one of F3's 600 body windows
/// idles, so its S comes from the calibration phase ([`Metrics::from_phases`]).
const MIN_BUSY_WINDOWS: usize = 3;

/// Whether `busy`, the windows of `all` that did not idle, resolve S ([`MIN_BUSY_WINDOWS`]).
pub(super) fn resolves_s(busy: usize, all: usize) -> bool {
    busy >= MIN_BUSY_WINDOWS || (busy > 0 && busy == all)
}

/// Largest busy share b of the idling windows' host time for c to be resolvable: a relative error
/// e in S becomes `e x b / (1 - b)` in c, so this caps it at 3x. Every F-suite that
/// idles lands between 0.55 and 0.75, because the guest's menu idle is cheap per window.
/// UNVERIFIED: a design choice.
const MAX_BUSY_PART_OF_IDLE: f64 = 0.75;

/// Busy share above which c carries its error-amplification note.
const NOISY_BUSY_PART_OF_IDLE: f64 = 0.5;

/// The metrics of one run.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Metrics {
    /// Busy speed S: executed instructions per host second of busy work, in MIPS. `None` when the
    /// run cannot resolve it ([`Metrics::notes`] says why).
    pub(super) busy_mips: Option<f64>,
    /// Idle cost c: host seconds per idle emulated second. `None` when the run cannot resolve it.
    pub(super) idle_cost: Option<f64>,
    /// Guest demand per window, in MIPS: 95th percentile (nearest rank) and maximum.
    pub(super) demand_p95_mips: f64,
    pub(super) demand_max_mips: f64,
    /// Host milliseconds of the slowest window.
    pub(super) worst_window_ms: f64,
    /// Virtual seconds per host second over the whole run.
    pub(super) real_time_factor: f64,
    pub(super) wall_s: f64,
    pub(super) virtual_s: f64,
    pub(super) busy_insns: u64,
    pub(super) windows: usize,
    /// Share of the CPU time of the windows S came from that ran on the performance cluster, or
    /// `None` when S is not resolvable or the host does not split CPU time by core class.
    pub(super) s_perf_share: Option<f64>,
    /// The same share over the measured windows, which every other host-time metric comes from.
    pub(super) perf_share: Option<f64>,
    /// Why an estimate is absent.
    pub(super) notes: Vec<String>,
}

/// Nearest-rank percentile `p` (0 to 100) of `values`; 0 for an empty slice.
pub(super) fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl Metrics {
    /// [`Metrics::from_phases`] with no calibration phase.
    pub(super) fn from_windows(windows: &[Window]) -> Metrics {
        Metrics::from_phases(windows, &[])
    }

    /// Derives the metrics of one run; an estimate the run cannot support is `None` with a note.
    /// The machine does not expose the host time of its idle skip, so S and c come from the cost
    /// model `host = insns / S + idle x c` (`bench/model.rs`). Inside one
    /// window the two terms are collinear, so S comes from windows that did not idle: the
    /// workload's own, else those of `calibration` (a boot in short slices,
    /// [`CALIBRATION_PS`](super::suites::CALIBRATION_PS)). c is what is left of the idling windows'
    /// host time at that S, per idle second, while their busy part is at most
    /// [`MAX_BUSY_PART_OF_IDLE`]. All else comes from `measured` alone.
    pub(super) fn from_phases(measured: &[Window], calibration: &[Window]) -> Metrics {
        let ns = 1e-9;
        let host_s = |w: &Window| w.host_ns as f64 * ns;
        fn not_idling(ws: &[Window]) -> Vec<&Window> {
            ws.iter()
                .filter(|w| w.idle_ps == 0 && w.busy_insns > 0)
                .collect()
        }
        let idling: Vec<&Window> = measured.iter().filter(|w| w.idle_ps > 0).collect();
        let busy_insns: u64 = measured.iter().map(|w| w.busy_insns).sum();
        let mut notes = Vec::new();

        let own = not_idling(measured);
        let extra = not_idling(calibration);
        let busy = if resolves_s(own.len(), measured.len()) {
            Some(own)
        } else if resolves_s(extra.len(), calibration.len()) {
            notes.push(format!(
                "S is measured on the calibration phase of the same run: {} of the workload's \
                 {} windows did not idle, {} of the calibration phase's {} did",
                own.len(),
                measured.len(),
                extra.len(),
                calibration.len()
            ));
            Some(extra)
        } else {
            notes.push(format!(
                "S not resolvable: {} of the workload's {} windows and {} of the calibration \
                 phase's {} did not idle; S needs at least {MIN_BUSY_WINDOWS} of a phase, or \
                 every window of one",
                own.len(),
                measured.len(),
                extra.len(),
                calibration.len()
            ));
            None
        };
        let inv_s = busy.as_ref().and_then(|busy| {
            let host: f64 = busy.iter().map(|w| host_s(w)).sum();
            let insns: f64 = busy.iter().map(|w| w.busy_insns as f64).sum();
            (host > 0.0 && insns > 0.0).then_some(host / insns)
        });
        // Where S was measured: the core class of exactly the windows it came from.
        let s_perf_share = inv_s
            .and(busy.as_ref())
            .and_then(|busy| cores::perf_share(busy.iter().map(|w| w.cores)));
        let windows = measured;

        let idle_cost = match inv_s {
            // No idle happened, so there is no host cost of idling to divide. This is F1: the
            // `official` boot to `Calling app_main()` is 124 ms of uninterrupted work.
            _ if idling.is_empty() => {
                notes.push(
                    "c is not defined for this workload: no window of it idled, so it spent no \
                     idle emulated second to charge host time to"
                        .to_string(),
                );
                None
            }
            None => {
                notes.push("c not resolvable: S is not".to_string());
                None
            }
            Some(inv_s) => {
                let host: f64 = idling.iter().map(|w| host_s(w)).sum();
                let busy_part: f64 = idling.iter().map(|w| w.busy_insns as f64 * inv_s).sum();
                let idle: f64 = idling.iter().map(|w| w.idle_ps as f64 * 1e-12).sum();
                let share = busy_part / host;
                if share > MAX_BUSY_PART_OF_IDLE {
                    notes.push(format!(
                        "c not resolvable: busy work is {:.0} % of the idling windows' host \
                         time, over the {:.0} % bound",
                        share * 100.0,
                        MAX_BUSY_PART_OF_IDLE * 100.0
                    ));
                    None
                } else {
                    if share > NOISY_BUSY_PART_OF_IDLE {
                        notes.push(format!(
                            "c is the remainder after {:.0} % of the idling windows' host time: \
                             a relative error e in S becomes {:.1}x e in c",
                            share * 100.0,
                            share / (1.0 - share)
                        ));
                    }
                    Some((host - busy_part) / idle)
                }
            }
        };

        let demand: Vec<f64> = windows
            .iter()
            .map(|w| w.busy_insns as f64 / (w.span_ps as f64 * 1e-12) / 1e6)
            .collect();
        let wall_s: f64 = windows.iter().map(host_s).sum();
        let virtual_s: f64 = windows.iter().map(|w| w.span_ps as f64 * 1e-12).sum();
        Metrics {
            busy_mips: inv_s.map(|v| 1.0 / v / 1e6),
            idle_cost,
            demand_p95_mips: percentile(&demand, 95.0),
            demand_max_mips: demand.iter().copied().fold(0.0, f64::max),
            worst_window_ms: windows
                .iter()
                .map(|w| w.host_ns as f64 * 1e-6)
                .fold(0.0, f64::max),
            real_time_factor: if wall_s > 0.0 {
                virtual_s / wall_s
            } else {
                0.0
            },
            wall_s,
            virtual_s,
            busy_insns,
            windows: windows.len(),
            s_perf_share,
            perf_share: cores::perf_share(windows.iter().map(|w| w.cores)),
            notes,
        }
    }

    /// Whether this run's host time was measured on the cores the native targets are for
    /// ([`Cluster::measures`]); no other run gates or baselines.
    pub(super) fn on_measured_cores(&self) -> bool {
        self.cluster().measures()
    }

    /// The core cluster the S phase and the measured phase ran on.
    pub(super) fn cluster(&self) -> Cluster {
        cores::classify(&[self.s_perf_share, self.perf_share])
    }

    pub(super) fn to_json(&self) -> Value {
        json!({
            "busy_mips": self.busy_mips,
            "idle_cost": self.idle_cost,
            "demand_p95_mips": self.demand_p95_mips,
            "demand_max_mips": self.demand_max_mips,
            "worst_window_ms": self.worst_window_ms,
            "real_time_factor": self.real_time_factor,
            "wall_s": self.wall_s,
            "virtual_s": self.virtual_s,
            "busy_insns": self.busy_insns,
            "windows": self.windows,
            "notes": self.notes,
        })
    }
}
