//! The hard native gates of the M5 and M6 perf budgets.

use super::metrics::Metrics;
use super::model::F3;

/// What a perf budget bounds on one workload.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum GateKind {
    /// Wall seconds of the measured phase at `Max`.
    WallSeconds(f64),
    /// Host milliseconds of the worst 100 ms virtual window of the measured phase.
    WorstWindowMs(f64),
    /// Idle cost c of the measured phase.
    IdleCost(f64),
}

/// One hard native gate on a workload, named by the budget that states it.
#[derive(Clone, Copy, Debug)]
pub(super) struct ExitGate {
    pub(super) workload: &'static str,
    pub(super) budget: &'static str,
    pub(super) kind: GateKind,
}

/// Every hard native gate of the M5 perf budget. Busy MIPS has no absolute native floor, so no gate
/// here names it: it is trend data, gated by [`regressions`](super::history::regressions).
pub(super) const EXIT_GATES: [ExitGate; 5] = [
    ExitGate {
        workload: "F1",
        budget: "M5 perf",
        kind: GateKind::WallSeconds(0.2),
    },
    ExitGate {
        workload: F3,
        budget: "M5 perf",
        kind: GateKind::WallSeconds(2.0),
    },
    ExitGate {
        workload: F3,
        budget: "M5 perf",
        kind: GateKind::IdleCost(0.02),
    },
    ExitGate {
        workload: "F4",
        budget: "M5 perf",
        kind: GateKind::WorstWindowMs(30.0),
    },
    ExitGate {
        workload: "F5",
        budget: "M5 perf",
        kind: GateKind::WorstWindowMs(30.0),
    },
];

/// The M6 audio gate, kept apart so each budget's rows are listed under its own name.
pub(super) const F6_GATES: [ExitGate; 1] = [ExitGate {
    workload: "F6",
    budget: "M6 perf",
    kind: GateKind::WorstWindowMs(30.0),
}];

/// Checks the gates of `gates` against `m`, one message per gate that fails.
pub(super) fn exit_gate_lines(
    gates: &[ExitGate],
    workload: &str,
    m: &Metrics,
    load: Option<f64>,
) -> (Vec<String>, Vec<String>) {
    let context = match load {
        Some(load) => format!(
            " (one-minute load average {load:.1} while it ran; a native target is an \
             absolute host-time budget, so a host running other work does not measure this)"
        ),
        None => String::new(),
    };
    let mut lines = Vec::new();
    let mut failures = Vec::new();
    for gate in gates.iter().filter(|g| g.workload == workload) {
        let (name, value, limit, unit) = match gate.kind {
            GateKind::WallSeconds(limit) => ("wall at Max", Some(m.wall_s), limit, "s"),
            GateKind::WorstWindowMs(limit) => {
                ("worst 100 ms window", Some(m.worst_window_ms), limit, "ms")
            }
            GateKind::IdleCost(limit) => ("idle cost c", m.idle_cost, limit, ""),
        };
        let Some(value) = value else {
            failures.push(format!(
                "{} {name}: the run could not resolve it, and {} gates on it",
                gate.workload, gate.budget
            ));
            continue;
        };
        let verdict = if value <= limit { "meets" } else { "MISSES" };
        lines.push(format!(
            "   {} gate {name:<20} {value:.4} {unit} against {limit} {unit}: {verdict}",
            gate.budget
        ));
        if value > limit {
            failures.push(format!(
                "{} {name} is {value:.4} {unit}, over the {} target of {limit} {unit}{context}",
                gate.workload, gate.budget
            ));
        }
    }
    (lines, failures)
}
