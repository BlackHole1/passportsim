//! Tests of `cargo xtask bench`.

use super::*;

use pemu_core::input::ButtonId;
use serde_json::{Value, json};

use super::cli::{Options, config_key, finish, selected};
use super::cores::{Cluster, CoreTime};
use super::gates::{EXIT_GATES, ExitGate, F6_GATES, GateKind, exit_gate_lines};
use super::history::{
    CONTENDED_LOAD_PER_CORE, Cores, Host, Record, baseline_exclusions, contention, cores,
    load_history, median, regressions, save_history,
};
use super::metrics::{Metrics, Window, percentile, resolves_s};
use super::model::{
    Engine, F3, F3_ROWS, F3Row, G1_F3, G1_WORST_WINDOW_MIPS, Limit, Measured, Mode, ModelInputs,
    RowVerdict, WINDOW_PS, browser_measurement, check_row, check_row_measured, f3_recorded,
    model_report, native_measurement, realtime_min_s,
};
use super::run::{ble_connect_script, ble_poll_script, repeats_done, reported_run, settled};
use super::suites::{CLICK_MS, OFFICIAL, PK, ROM_BOOT, SUITES, any_suite_runnable, suite_runnable};

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

// ---- the cost model reproduces the target arithmetic ----

#[test]
fn the_native_f3_row_splits_into_its_documented_parts() {
    let row = F3_ROWS[0];
    // "The idle term alone is 60 x 0.983 x 0.02 = 1.18 s, plus about 0.39 s busy" at the
    // spike's ~430 MIPS.
    let idle = row.predict(G1_F3, f64::INFINITY, 0.02);
    assert!(close(idle, 1.18, 0.005), "{idle}");
    let busy = row.predict(G1_F3, 430.0, 0.0);
    assert!(close(busy, 0.39, 0.005), "{busy}");
}

#[test]
fn a_one_second_f3_wall_with_c_002_is_unreachable_at_any_speed() {
    let one_second = F3Row {
        limit: Limit::WallSeconds(1.0),
        ..F3_ROWS[0]
    };
    match check_row(&one_second, G1_F3, Some(10_000.0)) {
        RowVerdict::UnreachableAtAnyS { idle_term } => assert!(close(idle_term, 1.1796, 1e-4)),
        other => panic!("{other:?}"),
    }
    // The alternative target for 1 s is c <= 0.007. The model allows up to 1 / 60 / 0.983 at
    // any speed and 0.01036 at the spike's 430 MIPS, so 0.007 sits inside the reachable
    // region (it is the bound at about 300 MIPS) and the alternative is consistent.
    let at_any = one_second.max_c(G1_F3, f64::INFINITY).unwrap();
    assert!(close(at_any, 0.016_954, 1e-6), "{at_any}");
    let at_430 = one_second.max_c(G1_F3, 430.0).unwrap();
    assert!(close(at_430, 0.010_355, 1e-6), "{at_430}");
    let keep_one_second = F3Row {
        c_max: 0.007,
        ..one_second
    };
    assert!(matches!(
        check_row(&keep_one_second, G1_F3, Some(430.0)),
        RowVerdict::Reachable { .. }
    ));
}

#[test]
fn the_browser_share_is_6_3_percent_so_5_percent_contradicted_c_005() {
    let row = F3_ROWS[1];
    let share = row.predict(G1_F3, 200.0, 0.05);
    assert!(close(share, 0.063, 0.0005), "{share}");
    let five = F3Row {
        limit: Limit::CoreShare(0.05),
        ..row
    };
    assert!(matches!(
        check_row(&five, G1_F3, Some(200.0)),
        RowVerdict::UnreachableAtS { .. }
    ));
    assert!(matches!(
        check_row(&row, G1_F3, Some(200.0)),
        RowVerdict::Reachable { .. }
    ));
    // The alternative target for 5 % is c <= 0.035 at S = 200; the model's exact bound is
    // 0.03667, so the rounded-down figure is inside the reachable region.
    let c = five.max_c(G1_F3, 200.0).unwrap();
    assert!(close(c, 0.036_673, 1e-6), "{c}");
    assert!(matches!(
        check_row(
            &F3Row {
                c_max: 0.035,
                ..five
            },
            G1_F3,
            Some(200.0)
        ),
        RowVerdict::Reachable { .. }
    ));
}

#[test]
fn min_s_is_exactly_where_the_row_turns_reachable() {
    let row = F3_ROWS[0];
    let min_s = row.min_s(G1_F3).unwrap();
    assert!(close(
        row.predict(G1_F3, min_s, row.c_max),
        row.limit(),
        1e-9
    ));
    assert!(matches!(
        check_row(&row, G1_F3, Some(min_s * 1.001)),
        RowVerdict::Reachable { .. }
    ));
    match check_row(&row, G1_F3, Some(min_s * 0.9)) {
        RowVerdict::UnreachableAtS {
            predicted,
            min_s: m,
        } => {
            assert!(predicted > row.limit());
            assert!(close(m, min_s, 1e-9));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        check_row(&row, G1_F3, None),
        RowVerdict::NotMeasured { .. }
    ));
}

#[test]
fn the_real_time_condition_gives_the_documented_speeds() {
    // "needs S >= 110 at esp32sim's wasm idle cost (c = 0.28), and S >= 100 at c = 0.05".
    let at_028 = realtime_min_s(G1_WORST_WINDOW_MIPS, 0.28).unwrap();
    let at_005 = realtime_min_s(G1_WORST_WINDOW_MIPS, 0.05).unwrap();
    assert!(close(at_028, 110.1, 0.1), "{at_028}");
    assert!(close(at_005, 100.1, 0.1), "{at_005}");
    assert_eq!(realtime_min_s(10.0, 1.1), None);
}

fn measured(s: f64, c: Option<f64>) -> Measured {
    Measured {
        s,
        c,
        source: "test".to_string(),
        stand_in: false,
    }
}

fn inputs(native: Option<Measured>, f3_runnable: bool) -> ModelInputs {
    ModelInputs {
        native,
        chrome: None,
        jsc: None,
        f3_runnable,
    }
}

#[test]
fn the_report_names_the_unreachable_row_and_the_overshoot() {
    let slow = inputs(Some(measured(50.0, Some(0.01))), true);
    let out = model_report(&F3_ROWS, G1_F3, &slow, Mode::Report);
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    let f = &out.failures[0];
    assert!(
        f.starts_with("F3 native wall at Max: unreachable at the measured S = 50.00"),
        "{f}"
    );
    // 60 x (2.79 / 50 + 0.983 x 0.02) = 4.5276 s, over 2 s by 2.528 s.
    assert!(
        f.contains("predicted 4.528 s") && f.contains("by 2.528 s"),
        "{f}"
    );
    let all = ModelInputs {
        chrome: Some(measured(200.0, Some(0.04))),
        jsc: Some(measured(200.0, Some(0.04))),
        ..inputs(Some(measured(430.0, Some(0.01))), true)
    };
    let out = model_report(&F3_ROWS, G1_F3, &all, Mode::Gate(Vec::new()));
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(
        out.summary(),
        "checked all 3 F3 rows against a measured S and c; none is unreachable under its own c"
    );
}

#[test]
fn a_measured_c_over_the_ceiling_fails_even_at_a_fast_s() {
    let row = F3_ROWS[0];
    // 60 x (2.79 / 500 + 0.983 x 0.03) = 2.104 s: the measured c breaks the row.
    match check_row_measured(&row, G1_F3, Some(500.0), Some(0.03)) {
        RowVerdict::MeasuredCOverCeiling { c, predicted } => {
            assert_eq!(c, 0.03);
            assert!(close(predicted, 2.1044, 1e-3), "{predicted}");
        }
        other => panic!("{other:?}"),
    }
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(Some(measured(500.0, Some(0.03))), true),
        Mode::Report,
    );
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert!(
        out.failures[0].contains("measured c = 0.03000")
            && out.failures[0].contains("ceiling 0.02"),
        "{}",
        out.failures[0]
    );
    // Under the ceiling it passes.
    assert!(matches!(
        check_row_measured(&row, G1_F3, Some(500.0), Some(0.015)),
        RowVerdict::Reachable { .. }
    ));
}

#[test]
fn a_stand_in_s_reports_before_f3_runs_but_an_unreachable_target_always_fails() {
    let proxy = Measured {
        stand_in: true,
        ..measured(37.0, None)
    };
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(Some(proxy.clone()), false),
        Mode::Report,
    );
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert!(
        out.lines
            .iter()
            .any(|l| l.contains("not failing, S is a stand-in")
                && l.contains("F3 native wall at Max: unreachable")),
        "{:?}",
        out.lines
    );
    // The success line never claims a check that did not run.
    assert!(
        !out.summary().contains("none is unreachable"),
        "{}",
        out.summary()
    );
    assert!(out.summary().contains("0 of 3"), "{}", out.summary());
    // A 1 s row with c <= 0.02 fails with no S at all.
    let old = [F3Row {
        limit: Limit::WallSeconds(1.0),
        ..F3_ROWS[0]
    }];
    let out = model_report(&old, G1_F3, &inputs(Some(proxy), false), Mode::Report);
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert!(
        out.failures[0].contains("unreachable at any busy speed")
            && out.failures[0].contains("1.180 s")
            && out.failures[0].contains("by 0.180 s"),
        "{}",
        out.failures[0]
    );
    let out = model_report(&old, G1_F3, &inputs(None, false), Mode::Report);
    assert_eq!(out.failures.len(), 1);
}

#[test]
fn once_f3_runs_a_stand_in_or_missing_native_s_fails() {
    let proxy = Measured {
        stand_in: true,
        ..measured(500.0, None)
    };
    let out = model_report(
        &F3_ROWS[..1],
        G1_F3,
        &inputs(Some(proxy), true),
        Mode::Report,
    );
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert!(out.failures[0].contains("stand-in"), "{}", out.failures[0]);
    let out = model_report(&F3_ROWS[..1], G1_F3, &inputs(None, true), Mode::Report);
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert!(
        out.failures[0].contains("no measured S"),
        "{}",
        out.failures[0]
    );
}

#[test]
fn gate_mode_fails_on_a_row_that_was_not_measured() {
    let native = Some(measured(430.0, Some(0.01)));
    let out = model_report(&F3_ROWS, G1_F3, &inputs(native.clone(), true), Mode::Report);
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert!(
        out.summary().contains("1 of 3") && out.summary().contains("2 not measured"),
        "{}",
        out.summary()
    );
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(native, true),
        Mode::Gate(Vec::new()),
    );
    assert_eq!(out.failures.len(), 2, "{:?}", out.failures);
    assert!(
        out.failures.iter().all(|f| f.contains("not measured")),
        "{:?}",
        out.failures
    );
    let proxy = Measured {
        stand_in: true,
        ..measured(500.0, Some(0.01))
    };
    let out = model_report(
        &F3_ROWS[..1],
        G1_F3,
        &inputs(Some(proxy), false),
        Mode::Gate(Vec::new()),
    );
    assert_eq!(
        out.failures.len(),
        1,
        "a stand-in never passes the gate: {:?}",
        out.failures
    );
}

// ---- metrics ----

fn busy(insns: u64, host_ms: u64) -> Window {
    Window {
        busy_insns: insns,
        idle_ps: 0,
        span_ps: WINDOW_PS,
        host_ns: host_ms * 1_000_000,
        cores: None,
    }
}

#[test]
fn busy_windows_give_s_and_idle_windows_give_c() {
    // S = 100 MIPS: 4 M instructions in 40 ms. Idle windows: 0.5 M instructions (5 ms) plus
    // 90 ms idle at c = 0.1 (9 ms), so 14 ms host; their busy part is about a third.
    let idle = Window {
        busy_insns: 500_000,
        idle_ps: 90_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: 14_000_000,
        cores: None,
    };
    let windows = [
        busy(4_000_000, 40),
        idle,
        busy(6_000_000, 60),
        idle,
        busy(5_000_000, 50),
    ];
    let m = Metrics::from_windows(&windows);
    assert!(
        close(m.busy_mips.unwrap(), 100.0, 1e-9),
        "{:?}",
        m.busy_mips
    );
    assert!(close(m.idle_cost.unwrap(), 0.1, 1e-9), "{:?}", m.idle_cost);
    assert!(close(m.demand_max_mips, 60.0, 1e-9));
    assert!(close(m.worst_window_ms, 60.0, 1e-9));
    assert!(close(m.virtual_s, 0.5, 1e-12));
    assert!(close(m.real_time_factor, 0.5 / 0.178, 1e-9));
    assert_eq!(m.busy_insns, 16_000_000);
    assert!(m.notes.is_empty(), "{:?}", m.notes);
}

#[test]
fn a_mostly_idle_run_reports_s_and_c_unresolvable_instead_of_failing() {
    // Every window idles: no busy window measures S, so neither S nor c is claimed, and the
    // demand and worst-window metrics are still reported.
    let w = |n: u64, idle_ms: u64, host_us: u64| Window {
        busy_insns: n,
        idle_ps: idle_ms * 1_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: host_us * 1_000,
        cores: None,
    };
    // Host time that falls as instructions rise: a least-squares fit takes a negative slope
    // here.
    let windows = [
        w(1_000_000, 90, 900),
        w(3_000_000, 40, 300),
        w(2_000_000, 60, 600),
    ];
    let m = Metrics::from_windows(&windows);
    assert_eq!(m.busy_mips, None);
    assert_eq!(m.idle_cost, None);
    assert!(
        m.notes.iter().any(|n| n.contains("S not resolvable")),
        "{:?}",
        m.notes
    );
    assert!(close(m.worst_window_ms, 0.9, 1e-9));
    assert!(close(m.demand_max_mips, 30.0, 1e-9));
}

#[test]
fn too_few_busy_windows_do_not_resolve_s() {
    let idle = Window {
        busy_insns: 10_000,
        idle_ps: 99_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: 2_000_000,
        cores: None,
    };
    let mut windows = vec![busy(4_000_000, 40), busy(4_000_000, 40)];
    windows.extend(std::iter::repeat_n(idle, 8));
    let m = Metrics::from_windows(&windows);
    assert_eq!((m.busy_mips, m.idle_cost), (None, None));
    windows.push(busy(4_000_000, 40));
    // Three busy windows of eleven: past both the count and the 10 % share.
    let m = Metrics::from_windows(&windows);
    assert!(close(m.busy_mips.unwrap(), 100.0, 1e-9));
    assert!(m.idle_cost.is_some());
}

#[test]
fn c_is_not_resolvable_when_busy_work_dominates_the_idling_windows() {
    let idle = Window {
        busy_insns: 9_000_000,
        idle_ps: 10_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: 91_000_000,
        cores: None,
    };
    let windows = [
        busy(4_000_000, 40),
        busy(4_000_000, 40),
        busy(4_000_000, 40),
        idle,
    ];
    let m = Metrics::from_windows(&windows);
    assert!(m.busy_mips.is_some());
    assert_eq!(m.idle_cost, None);
    assert!(
        m.notes.iter().any(|n| n.contains("c not resolvable")),
        "{:?}",
        m.notes
    );
}

#[test]
fn no_idle_means_no_c_and_no_instructions_means_no_s() {
    let m = Metrics::from_windows(&[busy(1_000, 1), busy(1_000, 1), busy(1_000, 1)]);
    assert!(m.busy_mips.is_some());
    assert_eq!(m.idle_cost, None);
    assert_eq!(Metrics::from_windows(&[busy(0, 1)]).busy_mips, None);
    assert_eq!(Metrics::from_windows(&[]).busy_mips, None);
}

#[test]
fn percentile_is_nearest_rank() {
    let v: Vec<f64> = (1..=20).map(f64::from).collect();
    assert_eq!(percentile(&v, 95.0), 19.0);
    assert_eq!(percentile(&v, 100.0), 20.0);
    assert_eq!(percentile(&[7.0], 95.0), 7.0);
    assert_eq!(percentile(&[], 95.0), 0.0);
}

// ---- the regression gate ----

fn host(cpu: &str) -> Host {
    Host {
        os: "macos".into(),
        arch: "aarch64".into(),
        cpu: cpu.into(),
    }
}

fn record(workload: &str, cpu: &str, mips: f64, c: Option<f64>) -> Record {
    let mut metrics = Metrics::from_windows(&[busy(1_000_000, 10)]);
    metrics.busy_mips = Some(mips);
    metrics.idle_cost = c;
    Record {
        workload: workload.into(),
        host: host(cpu),
        config: json!({ "executor": "test" }),
        metrics,
        commit: "c0ffee".into(),
        dirty: false,
        unix_s: 0,
        load_avg: None,
        cores: on(Cluster::Performance),
    }
}

/// The cores of a run at the user-interactive class on `cluster`.
fn on(cluster: Cluster) -> Cores {
    let share = match cluster {
        Cluster::Performance => Some(1.0),
        Cluster::Mixed => Some(0.7),
        Cluster::Efficiency => Some(0.0),
        Cluster::OneClass | Cluster::Unobserved => None,
    };
    Cores {
        qos: "user-interactive",
        cluster,
        s_share: share,
        share,
    }
}

/// `record` measured on `cluster`.
fn record_on(cluster: Cluster, mips: f64) -> Record {
    Record {
        cores: on(cluster),
        ..record("F6", "M3", mips, None)
    }
}

#[test]
fn a_record_compares_only_with_records_of_its_qos_class_and_core_cluster() {
    // The T2 failure `t2-272051f8-20260924T055032Z`: F6's S 220.85 against a rolling median
    // of 272.81, on a run the OS had spread over both clusters.
    let performance: Vec<(Record, bool)> = [281.2, 283.3, 281.7, 278.3, 272.8]
        .iter()
        .map(|&s| (record_on(Cluster::Performance, s), false))
        .collect();
    let h = history(&performance);
    let spread = record_on(Cluster::Mixed, 220.85);
    // The mix is still compared, so the loss is named ...
    let found = regressions(&h, &spread);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("busy_mips"), "{}", found[0]);
    // ... and it never becomes a baseline: a later performance run is judged against the
    // performance records alone.
    let mut h2 = h.clone();
    h2.push(spread.to_json(false));
    h2.push(spread.to_json(false));
    h2.push(spread.to_json(false));
    assert!(regressions(&h2, &record_on(Cluster::Performance, 260.0)).is_empty());
    assert_eq!(
        regressions(&h2, &record_on(Cluster::Performance, 240.0)).len(),
        1
    );

    // Another QoS class is another measurement, however close the numbers.
    let mut utility = record_on(Cluster::Performance, 100.0);
    utility.cores.qos = "utility";
    let h3 = history(&[(utility, false)]);
    assert!(regressions(&h3, &record_on(Cluster::Performance, 50.0)).is_empty());

    // A host that reports no split compares its unobserved records with each other only.
    let h4 = history(&[(record_on(Cluster::Unobserved, 100.0), false)]);
    assert_eq!(
        regressions(&h4, &record_on(Cluster::Unobserved, 50.0)).len(),
        1
    );
    assert!(regressions(&h4, &record_on(Cluster::Performance, 50.0)).is_empty());
}

#[test]
fn records_from_before_the_reading_are_excluded_and_the_exclusion_is_said() {
    let mut old = record_on(Cluster::Performance, 1000.0).to_json(false);
    old.as_object_mut()
        .expect("a record is an object")
        .remove("cores");
    let h = vec![old.clone(), old];
    let new = record_on(Cluster::Performance, 100.0);
    assert!(
        regressions(&h, &new).is_empty(),
        "an unclassified record is no baseline"
    );
    let lines = baseline_exclusions(&h, &new);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("2 earlier record(s)") && lines[0].contains("cannot be classified"),
        "{}",
        lines[0]
    );
    let mut h = h;
    h.push(record_on(Cluster::Mixed, 90.0).to_json(false));
    let lines = baseline_exclusions(&h, &new);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[1].starts_with("1 earlier record(s)"), "{}", lines[1]);
    // Nothing left out is nothing said.
    assert!(baseline_exclusions(&history(&[(new.clone(), false)]), &new).is_empty());
}

#[test]
fn a_run_spread_over_both_clusters_is_reported_and_enforces_nothing() {
    let o = Options::parse(&[]).expect("default options");
    let h = history(&[(record_on(Cluster::Performance, 275.0), false)]);
    let spread = record_on(Cluster::Mixed, 210.0);
    let out = finish(&o, spread.clone(), &h, &[]);
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(out.stored["regressed"], false);
    assert_eq!(out.stored["cores"]["cluster"], "mixed");
    // The same loss on the performance cluster fails and is stored regressed.
    let out = finish(&o, record_on(Cluster::Performance, 210.0), &h, &[]);
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert_eq!(out.stored["regressed"], true);
    assert!(spread.cores.unmeasured().unwrap().contains("70 %"));
    assert_eq!(on(Cluster::Performance).unmeasured(), None);
    // A run a launcher clamped to the efficiency cluster (`taskpolicy -b` reads S about 77)
    // is one kind of core, and not the kind the targets are for: reported, not enforced, and
    // never a baseline.
    let clamped = record_on(Cluster::Efficiency, 77.0);
    let out = finish(&o, clamped.clone(), &h, &[]);
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert!(
        clamped
            .cores
            .unmeasured()
            .unwrap()
            .starts_with("core cluster efficiency")
    );
    let h = history(&[(clamped.clone(), false)]);
    assert!(regressions(&h, &clamped).is_empty());
    assert!(regressions(&h, &record_on(Cluster::Performance, 10.0)).is_empty());
    assert_eq!(on(Cluster::OneClass).unmeasured(), None);
    assert_eq!(on(Cluster::Unobserved).unmeasured(), None);
}

#[test]
fn the_reported_run_is_the_fastest_on_the_performance_cluster() {
    // A run's cluster is classified against this host's core classes, and only a host with
    // two of them has runs that are not measurements.
    if cores::core_classes().is_none_or(|n| n <= 1) {
        return;
    }
    let run = |wall: f64, s_share: f64| {
        let mut m = Metrics::from_windows(&[busy(1_000_000, 10)]);
        m.wall_s = wall;
        m.s_perf_share = Some(s_share);
        m.perf_share = Some(1.0);
        m
    };
    let (fast_mixed, slow, slower) = (run(0.19, 0.6), run(0.20, 1.0), run(0.21, 0.99));
    assert_eq!(reported_run(&[&fast_mixed, &slower, &slow]), 2);
    // Every run spread over both clusters: the fastest of all, which carries `mixed`.
    let also_mixed = run(0.18, 0.5);
    assert_eq!(reported_run(&[&fast_mixed, &also_mixed]), 1);
    // The walls agree and one run is on the performance cluster: done. Agreement alone is
    // not.
    assert!(repeats_done(&[&slow, &slow, &slower]));
    assert!(!repeats_done(&[&fast_mixed, &fast_mixed, &fast_mixed]));
    assert!(repeats_done(&[&fast_mixed, &fast_mixed, &slow]));
    assert!(!repeats_done(&[&slow, &slow]), "fewer than the minimum");
}

#[test]
fn s_carries_the_core_share_of_exactly_the_windows_it_came_from() {
    let on_cores = |w: Window, all: u64, perf: u64| Window {
        cores: Some(CoreTime { all, perf }),
        ..w
    };
    let idling = Window {
        busy_insns: 300_000,
        idle_ps: 98_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: 4_000_000,
        cores: None,
    };
    // The body idles in every window, so S comes from the calibration phase, and its share is
    // the calibration's (half), not the body's (all).
    let body = vec![on_cores(idling, 4, 4); 20];
    let calibration = vec![on_cores(busy(1_000_000, 10), 10, 5); 5];
    let m = Metrics::from_phases(&body, &calibration);
    assert_eq!(m.s_perf_share, Some(0.5));
    assert_eq!(m.perf_share, Some(1.0));
    assert_eq!(m.cluster(), cores::classify(&[Some(0.5), Some(1.0)]));
    // A phase with a window the host did not read has no share at all.
    let mut gap = body.clone();
    gap[3].cores = None;
    assert_eq!(Metrics::from_phases(&gap, &calibration).perf_share, None);
    // No S, no S share.
    assert_eq!(Metrics::from_phases(&body, &[]).s_perf_share, None);
}

fn history(records: &[(Record, bool)]) -> Vec<Value> {
    records
        .iter()
        .map(|(r, regressed)| r.to_json(*regressed))
        .collect()
}

#[test]
fn a_first_record_has_no_baseline_and_passes() {
    assert!(regressions(&[], &record("F1", "M3", 10.0, Some(1.0))).is_empty());
}

#[test]
fn busy_mips_more_than_10_percent_down_fails_and_9_percent_passes() {
    let h = history(&[(record("F1", "M3", 100.0, None), false)]);
    assert!(regressions(&h, &record("F1", "M3", 91.0, None)).is_empty());
    let found = regressions(&h, &record("F1", "M3", 89.0, None));
    assert_eq!(found.len(), 1);
    assert!(
        found[0].contains("busy_mips") && found[0].contains("-11.0 %"),
        "{}",
        found[0]
    );
    assert!(regressions(&h, &record("F1", "M3", 150.0, None)).is_empty());
}

#[test]
fn idle_cost_more_than_10_percent_up_fails() {
    let h = history(&[(record("F3", "M3", 100.0, Some(0.020)), false)]);
    assert!(regressions(&h, &record("F3", "M3", 100.0, Some(0.0219))).is_empty());
    let found = regressions(&h, &record("F3", "M3", 100.0, Some(0.0221)));
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("idle_cost"), "{}", found[0]);
    // A run that could not resolve c against a baseline that has one fails: an estimate that
    // disappears must not hide a regression.
    let found = regressions(&h, &record("F3", "M3", 100.0, None));
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("could not resolve"), "{}", found[0]);
    // A key that never had c is not compared on c.
    let h = history(&[(record("F3", "M3", 100.0, None), false)]);
    assert!(regressions(&h, &record("F3", "M3", 100.0, None)).is_empty());
}

#[test]
fn other_hosts_workloads_configs_and_regressed_records_are_not_baselines() {
    let mut other_config = record("F1", "M3", 1000.0, None);
    other_config.config = json!({ "executor": "engine" });
    let h = history(&[
        (record("F1", "Intel", 1000.0, None), false),
        (record("F5", "M3", 1000.0, None), false),
        (other_config, false),
        (record("F1", "M3", 1000.0, None), true),
        (record("F1", "M3", 100.0, None), false),
    ]);
    assert!(regressions(&h, &record("F1", "M3", 95.0, None)).is_empty());
}

#[test]
fn a_contended_record_is_not_a_baseline_and_an_accepted_one_still_is() {
    let contended = |mips: f64| {
        let mut r = record("F1", "M3", mips, None);
        r.load_avg = Some(cores() * CONTENDED_LOAD_PER_CORE * 4.0);
        r
    };
    assert!(contention(contended(100.0).load_avg).is_some());
    assert_eq!(contention(Some(0.0)), None, "an idle host is measuring");
    assert_eq!(contention(None), None, "an unknown load is not contention");

    // A run that shared the cores looks slow; it must not become the number a later quiet run
    // is judged against, or a busy afternoon would ratchet the baseline down for good.
    let h = history(&[(contended(100.0), false)]);
    assert!(h[0]["contended"] == true);
    assert!(regressions(&h, &record("F1", "M3", 200.0, None)).is_empty());
    assert!(regressions(&h, &record("F1", "M3", 50.0, None)).is_empty());

    // Pinning one on purpose is still a human decision the gate honours.
    let mut pinned = history(&[(contended(100.0), false)]);
    pinned[0] = contended(100.0).to_json_with(false, true);
    assert_eq!(
        regressions(&pinned, &record("F1", "M3", 50.0, None)).len(),
        1
    );
}

#[test]
fn the_baseline_is_the_median_of_the_last_five_passing_records() {
    // One old fast outlier falls out of the window of five; one recent noisy one is outvoted.
    let rows: Vec<(Record, bool)> = [500.0, 100.0, 100.0, 300.0, 100.0, 100.0]
        .iter()
        .map(|&s| (record("F1", "M3", s, None), false))
        .collect();
    let h = history(&rows);
    assert!(regressions(&h, &record("F1", "M3", 91.0, None)).is_empty());
    assert_eq!(regressions(&h, &record("F1", "M3", 89.0, None)).len(), 1);
    assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0]), 2.5);
}

#[test]
fn an_accepted_record_pins_a_new_baseline() {
    let mut h = history(&[
        (record("F1", "M3", 100.0, None), false),
        (record("F1", "M3", 70.0, None), true),
    ]);
    // Without an acceptance the lasting loss keeps failing.
    assert_eq!(regressions(&h, &record("F1", "M3", 70.0, None)).len(), 1);
    h.push(record("F1", "M3", 70.0, None).to_json_with(false, true));
    assert!(regressions(&h, &record("F1", "M3", 70.0, None)).is_empty());
    assert!(regressions(&h, &record("F1", "M3", 64.0, None)).is_empty());
    let found = regressions(&h, &record("F1", "M3", 62.0, None));
    assert!(
        !found.is_empty(),
        "62 is more than 10 % under the accepted 70"
    );
}

#[test]
fn slow_drift_fails_against_the_anchor_once_it_adds_up_to_10_percent() {
    // Each run about 3 % under the one before: every step stays inside the rolling band.
    let h = history(&[
        (record("F1", "M3", 100.0, None), false),
        (record("F1", "M3", 97.0, None), false),
        (record("F1", "M3", 94.1, None), false),
        (record("F1", "M3", 91.3, None), false),
        (record("F1", "M3", 88.6, None), false),
        (record("F1", "M3", 85.9, None), false),
    ]);
    // Rolling: median(97, 94.1, 91.3, 88.6, 85.9) = 91.3, bound 82.17, so 84.0 passes it.
    // Anchor: median of the first five = 94.1, bound 84.69, so 84.0 fails.
    let found = regressions(&h, &record("F1", "M3", 84.0, None));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("anchor"), "{}", found[0]);
    assert!(regressions(&h, &record("F1", "M3", 85.0, None)).is_empty());
}

#[test]
fn a_zero_idle_cost_baseline_is_gated_by_an_absolute_band() {
    let h = history(&[(record("F3", "M3", 100.0, Some(0.0)), false)]);
    // Within 0.002 of a zero baseline passes, beyond it fails.
    assert!(regressions(&h, &record("F3", "M3", 100.0, Some(0.0019))).is_empty());
    let found = regressions(&h, &record("F3", "M3", 100.0, Some(0.5)));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("idle_cost"), "{}", found[0]);
    // A small baseline is gated by the absolute band, not the 10 % one: 0.001 + 0.002.
    let h = history(&[(record("F3", "M3", 100.0, Some(0.001)), false)]);
    assert!(regressions(&h, &record("F3", "M3", 100.0, Some(0.0029))).is_empty());
    assert_eq!(
        regressions(&h, &record("F3", "M3", 100.0, Some(0.0031))).len(),
        1
    );
}

#[test]
fn history_round_trips_and_refuses_another_version() {
    let dir = std::env::temp_dir().join(format!("pemu-bench-test-{}", std::process::id()));
    let path = dir.join("bench/history.json");
    assert!(load_history(&path).unwrap().is_empty());
    let h = history(&[(record("F1", "M3", 100.0, Some(0.01)), false)]);
    save_history(&path, &h).unwrap();
    let once = load_history(&path).unwrap();
    assert_eq!(once.len(), 1);
    assert_eq!(once[0]["workload"], "F1");
    assert_eq!(once[0]["metrics"]["busy_mips"].as_f64(), Some(100.0));
    assert_eq!(once[0]["metrics"]["idle_cost"].as_f64(), Some(0.01));
    // JSON text is the stable form: a second round trip changes nothing.
    save_history(&path, &once).unwrap();
    assert_eq!(load_history(&path).unwrap(), once);
    std::fs::write(&path, r#"{"version": 2, "records": []}"#).unwrap();
    assert!(load_history(&path).unwrap_err().contains("version"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn check_model_prefers_f3_even_regressed_then_names_rom_boot_as_a_stand_in() {
    let me = host("M3");
    let h = history(&[(record(ROM_BOOT, "M3", 36.0, None), true)]);
    let m = native_measurement(&h, &me, None, None).unwrap();
    assert_eq!(m.s, 36.0);
    assert!(m.source.contains("standing in for F3"), "{}", m.source);
    assert!(m.stand_in);
    // A regressed F3 record is still the newest F3 measurement: it must not be skipped for
    // an older record or the stand-in.
    let h = history(&[
        (record(ROM_BOOT, "M3", 36.0, None), false),
        (record(F3, "M3", 400.0, Some(0.01)), false),
        (record(F3, "M3", 300.0, Some(0.03)), true),
    ]);
    let m = native_measurement(&h, &me, None, None).unwrap();
    assert_eq!((m.s, m.c), (300.0, Some(0.03)));
    assert!(!m.stand_in);
    assert!(f3_recorded(&h, &me));
    assert_eq!(native_measurement(&h, &host("Intel"), None, None), None);
    let m = native_measurement(&[], &me, Some(7.0), Some(0.01)).unwrap();
    assert_eq!((m.s, m.c), (7.0, Some(0.01)));
}

#[test]
fn a_browser_f3_record_is_never_the_native_s() {
    let me = host("M3");
    let mut browser = record(F3, "M3", 20.0, Some(0.04)).to_json_with(false, false);
    browser["config"]["runtime"] = json!("chrome");
    let mut h = history(&[(record(F3, "M3", 400.0, Some(0.01)), false)]);
    h.push(browser.clone());
    let m = native_measurement(&h, &me, None, None).unwrap();
    assert_eq!((m.s, m.c), (400.0, Some(0.01)));
    assert!(!f3_recorded(&[browser], &me));
}

#[test]
fn check_model_takes_the_browser_s_from_the_newest_uncontended_browser_record() {
    let me = host("M3");
    let browser = |engine: &str, s: f64, load: Option<f64>| {
        let mut r = record(F3, "M3", s, Some(0.013));
        r.load_avg = load;
        let mut v = r.to_json_with(false, false);
        v["config"]["runtime"] = json!(engine);
        v
    };
    let h = vec![
        browser("chrome", 180.0, Some(1.0)),
        browser("jsc", 175.0, Some(1.0)),
        // Newer, but taken on a host with more runnable threads than cores.
        browser("chrome", 90.0, Some(10_000.0)),
        record(F3, "M3", 400.0, Some(0.01)).to_json(false),
    ];
    let chrome = browser_measurement(&h, &me, Engine::Chrome, None, None).unwrap();
    assert_eq!((chrome.s, chrome.c), (180.0, Some(0.013)));
    assert!(chrome.source.contains("chrome"), "{}", chrome.source);
    assert_eq!(
        browser_measurement(&h, &me, Engine::Jsc, None, None)
            .unwrap()
            .s,
        175.0
    );
    assert_eq!(
        browser_measurement(&h, &host("Intel"), Engine::Jsc, None, None),
        None
    );
    // The flag wins, and the native S never comes from a browser record.
    let flagged = browser_measurement(&h, &me, Engine::Jsc, Some(7.0), None).unwrap();
    assert_eq!((flagged.s, flagged.source.as_str()), (7.0, "--s-jsc"));
    assert_eq!(native_measurement(&h, &me, None, None).unwrap().s, 400.0);
}

#[test]
fn an_f3_record_without_a_resolvable_s_does_not_fall_back_to_the_stand_in() {
    let me = host("M3");
    let mut f3 = record(F3, "M3", 1.0, None);
    f3.metrics.busy_mips = None;
    let h = history(&[(record(ROM_BOOT, "M3", 36.0, None), false), (f3, false)]);
    assert_eq!(native_measurement(&h, &me, None, None), None);
    assert!(f3_recorded(&h, &me));
}

#[test]
fn the_config_key_changes_with_every_input_that_changes_timings() {
    let base = config_key(64, false, false, "Fast", false, "ab", 10);
    assert_eq!(base, config_key(64, false, false, "Fast", false, "ab", 10));
    for other in [
        config_key(1, false, false, "Fast", false, "ab", 10),
        config_key(64, true, false, "Fast", false, "ab", 10),
        config_key(64, false, true, "Fast", false, "ab", 10),
        config_key(64, false, false, "Device", false, "ab", 10),
        config_key(64, false, false, "Fast", true, "ab", 10),
        config_key(64, false, false, "Fast", false, "cd", 10),
        config_key(64, false, false, "Fast", false, "ab", 20),
    ] {
        assert_ne!(base, other);
    }
    let stored = record("F1", "M3", 1.0, None).to_json(false);
    assert_eq!(stored["dirty"], false);
}

// ---- the F-suite table and its scenarios ----

#[test]
fn every_suite_is_declared_once_and_all_seven_run() {
    let ids: Vec<&str> = SUITES.iter().map(|s| s.id).collect();
    assert_eq!(ids, ["F1", "F2", F3, "F4", "F5", "F6", "F7"]);
    assert!(any_suite_runnable());
    for id in ["F1", "F2", F3, "F4", "F5", "F6", "F7"] {
        assert!(suite_runnable(id), "{id} runs");
    }
}

/// F7 is the only suite with a BLE leg, and the leg is the whole of what F7 asks the harness
/// for: advertising (the boot's own), one connection (the connect script of the
/// setup) and a notify stream over the measured body.
#[test]
fn f7_is_the_ble_suite_and_its_stream_covers_its_body() {
    for suite in &SUITES {
        let Some(s) = suite.scenario() else { continue };
        assert_eq!(
            s.ble.is_some(),
            suite.id == "F7",
            "{} is the BLE suite iff it is F7",
            suite.id
        );
    }
    let s = SUITES[6].scenario().expect("F7 runs");
    let leg = s.ble.expect("F7 has a BLE leg");
    assert_eq!(s.image.id, "pk");
    assert_eq!(s.body_ms, 30_000, "the row says 30 s");
    assert!(
        !s.boot_is_the_workload && s.setup_ms > 0,
        "the connection is setup and the stream is the body"
    );
    // The connect script must fit in one journaled script, and the stream must be polled over
    // the whole body rather than once at the start.
    assert!(ble_connect_script(leg).len() <= pemu_radio::ble::central::MAX_STEPS);
    assert_eq!(ble_poll_script(leg).len(), 2);
    assert!(
        leg.poll_period_ms > 0 && leg.poll_period_ms * 4 <= s.body_ms,
        "the body holds several polls"
    );
    assert!(
        leg.connect_within_ms as u64 + leg.scan_ms as u64 <= s.setup_ms,
        "the connect script has to finish inside the setup"
    );
}

#[test]
fn each_scenario_measures_the_phase_its_gate_is_defined_on() {
    for suite in &SUITES {
        let Some(s) = suite.scenario() else { continue };
        let gated = EXIT_GATES
            .iter()
            .chain(F6_GATES.iter())
            .filter(|g| g.workload == suite.id);
        for gate in gated {
            // "after boot" gates need a body to measure; wall gates work either way.
            if matches!(gate.kind, GateKind::WorstWindowMs(_)) {
                assert!(
                    !s.boot_is_the_workload && s.body_ms > 0,
                    "{} is gated on its worst window after boot, so it needs a body",
                    suite.id
                );
            }
        }
        if s.boot_is_the_workload {
            assert_eq!(s.body_ms, 0, "{}", suite.id);
            assert!(s.clicks.schedule().is_empty(), "{}", suite.id);
        }
    }
}

#[test]
fn the_click_schedules_are_the_workload_definitions() {
    // F4 is "40 menu clicks at 400 ms", and its body outlasts the last of them.
    let f4 = SUITES[3].scenario().expect("F4 runs");
    let clicks = f4.clicks.schedule();
    assert_eq!(clicks.len(), 40);
    assert_eq!(clicks[0], (0, ButtonId::Down));
    assert_eq!(clicks[39], (39 * 400, ButtonId::Down));
    assert!(f4.body_ms > 39 * 400 + CLICK_MS);
    // F5 enters the Display card, which the settled menu already selects, in its first
    // measured window: the card entry is what the M5 worst-window gate measures.
    let f5 = SUITES[4].scenario().expect("F5 runs");
    assert_eq!(f5.clicks.schedule(), [(0, ButtonId::Ok)]);
    assert_eq!(f5.setup.schedule(), []);
    // F6 walks to the Audio card in its setup and plays the measured tone in its body.
    let f6 = SUITES[5].scenario().expect("F6 runs");
    assert_eq!(
        f6.setup.schedule(),
        [
            (0, ButtonId::Down),
            (400, ButtonId::Down),
            (800, ButtonId::Ok),
            (1_200, ButtonId::Ok)
        ]
    );
    assert_eq!(f6.clicks.schedule(), [(0, ButtonId::Ok)]);
    assert!(f6.capture_audio, "the M6 budget wants F6's WAV artifact");
    assert!(!SUITES[4].scenario().unwrap().capture_audio);
}

#[test]
fn repeating_stops_when_the_runs_agree_and_not_while_they_scatter() {
    // Fewer than the minimum: keep going whatever they say.
    assert!(!settled(&[]));
    assert!(!settled(&[1.0, 1.0]));
    // A quiet host: three runs within 10 % of the fastest.
    assert!(settled(&[1.0, 1.05, 1.09]));
    assert!(settled(&[1.2, 1.0, 1.05, 1.1]));
    // One of the last three outside the band keeps it going.
    assert!(!settled(&[1.0, 1.05, 1.11]));
    // A contended host scatters rather than plateauing, so it spends its whole budget: a
    // plateau of slow runs is not agreement with a fast one that came before.
    assert!(!settled(&[9.0, 1.5, 5.4, 2.4]));
    assert!(!settled(&[1.5, 9.0, 9.0, 9.0]));
    // Runs that are all slow and all alike do agree: nothing says the host is at fault.
    assert!(settled(&[9.0, 9.0, 9.0]));
}

#[test]
fn a_short_workload_that_never_idles_still_resolves_s() {
    // F1 is two windows long and both are busy, which is every window of the phase.
    assert!(resolves_s(2, 2));
    assert!(!resolves_s(2, 5));
    assert!(resolves_s(3, 10));
    assert!(!resolves_s(0, 0));
    let m = Metrics::from_windows(&[busy(4_000_000, 40), busy(6_000_000, 60)]);
    assert!(close(m.busy_mips.unwrap(), 100.0, 1e-9), "{m:?}");
    assert_eq!(m.idle_cost, None);
    assert!(
        m.notes.iter().any(|n| n.contains("c is not defined")),
        "{:?}",
        m.notes
    );
}

#[test]
fn a_mostly_idle_workload_takes_s_from_the_calibration_phase() {
    let idling = Window {
        busy_insns: 300_000,
        idle_ps: 98_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: 4_000_000,
        cores: None,
    };
    let body = vec![idling; 20];
    // Alone the body resolves nothing: no window of it is busy.
    let alone = Metrics::from_phases(&body, &[]);
    assert_eq!((alone.busy_mips, alone.idle_cost), (None, None));
    // With a busy calibration phase S is 100 MIPS, and c follows from it: each window spends
    // 3 ms on its 300 k instructions and 1 ms on 98 ms of idle, so c is about 0.0102.
    let calibration = vec![busy(1_000_000, 10); 5];
    let m = Metrics::from_phases(&body, &calibration);
    assert!(close(m.busy_mips.unwrap(), 100.0, 1e-9), "{m:?}");
    assert!(close(m.idle_cost.unwrap(), 0.010_204, 1e-6), "{m:?}");
    assert!(
        m.notes
            .iter()
            .any(|n| n.contains("measured on the calibration phase")),
        "{:?}",
        m.notes
    );
    // The body's own windows are what the reported quantities come from, not the
    // calibration's: 20 windows of 4 ms, not 25 windows.
    assert_eq!(m.windows, 20);
    assert!(close(m.worst_window_ms, 4.0, 1e-9));
    assert!(close(m.wall_s, 0.08, 1e-9));
}

#[test]
fn c_is_refused_when_the_busy_term_would_swamp_it_and_noted_when_it_is_large() {
    let window = |insns: u64, host_ms: f64| Window {
        busy_insns: insns,
        idle_ps: 90_000_000_000,
        span_ps: WINDOW_PS,
        host_ns: (host_ms * 1e6) as u64,
        cores: None,
    };
    let calibration = vec![busy(1_000_000, 10); 5];
    // 600 k instructions cost 6 ms at 100 MIPS; a 10 ms window leaves 4 ms for 90 ms of
    // idle, a 60 % busy share: resolvable, with the amplification note.
    let m = Metrics::from_phases(&vec![window(600_000, 10.0); 10], &calibration);
    assert!(close(m.idle_cost.unwrap(), 0.044_444, 1e-6), "{m:?}");
    assert!(
        m.notes.iter().any(|n| n.contains("1.5x e in c")),
        "{:?}",
        m.notes
    );
    // 800 k instructions in a 10 ms window is an 80 % busy share: over the bound.
    let m = Metrics::from_phases(&vec![window(800_000, 10.0); 10], &calibration);
    assert_eq!(m.idle_cost, None);
    assert!(
        m.notes.iter().any(|n| n.contains("over the 75 % bound")),
        "{:?}",
        m.notes
    );
}

// ---- the hard native gates of the M5 and M6 perf budgets ----

#[test]
fn the_exit_gates_are_the_rows_of_the_m5_and_m6_budgets() {
    let all: Vec<ExitGate> = EXIT_GATES.iter().chain(F6_GATES.iter()).copied().collect();
    let of = |id: &str| -> Vec<GateKind> {
        all.iter()
            .filter(|g| g.workload == id)
            .map(|g| g.kind)
            .collect()
    };
    assert_eq!(of("F1"), [GateKind::WallSeconds(0.2)]);
    assert_eq!(
        of(F3),
        [GateKind::WallSeconds(2.0), GateKind::IdleCost(0.02)]
    );
    assert_eq!(of("F4"), [GateKind::WorstWindowMs(30.0)]);
    assert_eq!(of("F5"), [GateKind::WorstWindowMs(30.0)]);
    assert_eq!(of("F6"), [GateKind::WorstWindowMs(30.0)]);
    // No gate names busy MIPS: it has no absolute native floor.
    assert!(all.iter().all(|g| !matches!(
        g.kind,
        GateKind::WallSeconds(_) if g.workload == "F2"
    )));
    assert_eq!(all.iter().filter(|g| g.budget == "M6 perf").count(), 1);
}

#[test]
fn a_gate_fails_on_the_value_that_misses_and_on_one_the_run_could_not_resolve() {
    let mut m = Metrics::from_windows(&[busy(1_000_000, 10)]);
    m.wall_s = 2.5;
    m.idle_cost = Some(0.01);
    let (lines, failures) = exit_gate_lines(&EXIT_GATES, F3, &m, Some(1.0));
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(
        failures[0].contains("wall at Max is 2.5000 s"),
        "{failures:?}"
    );
    m.wall_s = 1.0;
    assert!(exit_gate_lines(&EXIT_GATES, F3, &m, Some(1.0)).1.is_empty());
    // A c the run could not resolve fails the row that gates on it.
    m.idle_cost = None;
    let (_, failures) = exit_gate_lines(&EXIT_GATES, F3, &m, Some(1.0));
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert!(failures[0].contains("could not resolve"), "{failures:?}");
    // A workload with no gate has nothing to fail.
    assert!(
        exit_gate_lines(&EXIT_GATES, ROM_BOOT, &m, None)
            .1
            .is_empty()
    );
}

#[test]
fn gate_engine_narrows_the_gate_to_the_engines_that_can_be_measured() {
    let native = Some(measured(430.0, Some(0.01)));
    // Gating every engine fails the two browser rows, which have no measurement here.
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(native.clone(), true),
        Mode::Gate(Vec::new()),
    );
    assert_eq!(out.failures.len(), 2, "{:?}", out.failures);
    // Gating the native engine alone passes: the browser rows are reported.
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(native.clone(), true),
        Mode::Gate(vec![Engine::Native]),
    );
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    // And it still fails when the native row is the one without a measurement.
    let out = model_report(
        &F3_ROWS,
        G1_F3,
        &inputs(None, false),
        Mode::Gate(vec![Engine::Native]),
    );
    assert_eq!(out.failures.len(), 1, "{:?}", out.failures);
    assert_eq!(Engine::parse("jsc").unwrap(), Engine::Jsc);
    assert!(Engine::parse("node").is_err());
}

#[test]
fn options_refuse_even_repeats_and_bad_speeds() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(Options::parse(&args(&["--repeat", "0"])).is_err());
    assert_eq!(Options::parse(&args(&["--repeat", "4"])).unwrap().repeat, 4);
    assert!(Options::parse(&args(&["--s-native", "-3"])).is_err());
    assert!(Options::parse(&args(&["--bogus"])).is_err());
    assert!(Options::parse(&args(&["--accept", "--no-record"])).is_err());
    assert!(Options::parse(&args(&["--accept"])).unwrap().accept);
    let o = Options::parse(&args(&["--check-model", "--s-chrome", "210"])).unwrap();
    assert!(o.check_model);
    assert_eq!(o.s_chrome, Some(210.0));
}

#[test]
fn workload_selection_names_the_ids_and_refuses_the_rest() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let o = Options::parse(&args(&["--workload", "F1,F3"])).unwrap();
    assert_eq!(o.workloads, ["F1", "F3"]);
    assert!(selected(&o, "F1") && selected(&o, F3));
    assert!(!selected(&o, "F5") && !selected(&o, ROM_BOOT));
    // `all` and no `--workload` both run everything.
    let o = Options::parse(&args(&["--workload", "all"])).unwrap();
    assert!(o.workloads.is_empty());
    assert!(selected(&o, "F7") && selected(&o, ROM_BOOT));
    assert!(selected(&Options::parse(&[]).unwrap(), "F6"));
    assert!(Options::parse(&args(&["--workload", "F9"])).is_err());
    assert!(Options::parse(&args(&["--gate-engine", "native"])).is_err());
    assert!(
        Options::parse(&args(&[
            "--check-model",
            "--gate",
            "--gate-engine",
            "native"
        ]))
        .is_ok()
    );
    assert!(Options::parse(&args(&["--check-model", "--gate-exits"])).is_err());
    assert!(Options::parse(&args(&["--gate-exits"])).unwrap().gate_exits);
}

#[test]
fn the_suite_key_separates_two_workloads_of_one_image() {
    // The key is JSON, so the shape is testable without a machine: two scenarios of the same
    // image and different bodies never baseline each other.
    let f5 = SUITES[4].scenario().unwrap();
    let f6 = SUITES[5].scenario().unwrap();
    assert_ne!(f5.body_ms, 0);
    assert_ne!(
        json!({"id": "F5", "body_ms": f5.body_ms, "clicks": f5.clicks.schedule().len()}),
        json!({"id": "F6", "body_ms": f6.body_ms, "clicks": f6.clicks.schedule().len()})
    );
    assert_eq!(f5.image, f6.image);
    assert_ne!(OFFICIAL, PK);
}
