//! Wall-clock pacing of a real-time run: the sleep-and-correct loop that keeps guest time near
//! host time.
//!
//! The wait is `std::thread::sleep` on both hosts and never a spin. Since Rust 1.75 the Windows
//! sleep uses a `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION` timer (about 0.5 ms), so neither host
//! needs `timeBeginPeriod(1)` or a bespoke timer; [`measure_wake_lateness`] is the evidence.
//! The browser paces with `Atomics.wait` in its Worker instead, because page timers are clamped.

use std::time::{Duration, Instant};

/// How far the guest may fall behind wall time before the pacer re-anchors. Re-anchoring runs the
/// guest in slow motion instead of sprinting through the backlog, so virtual time never jumps and
/// a paced session equals its journal replayed in `Max`.
pub const MAX_LAG: Duration = Duration::from_millis(250);

/// Sleeps until `deadline`, sleeping again after an early wake. A deadline already past returns
/// at once without yielding.
pub fn sleep_until(deadline: Instant) {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return;
        };
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(remaining);
    }
}

/// What the pacer decided for one slice boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pace {
    Sleep(Duration),
    /// The guest is behind by less than [`MAX_LAG`]: run the next slice at once.
    Run {
        behind: Duration,
    },
    /// The guest fell more than [`MAX_LAG`] behind and the anchors moved to where it is.
    Reanchored {
        behind: Duration,
    },
}

impl Pace {
    pub fn sleep(self) -> Duration {
        match self {
            Pace::Sleep(d) => d,
            Pace::Run { .. } | Pace::Reanchored { .. } => Duration::ZERO,
        }
    }
}

/// The wall-pacing loop, targeting `vt_anchor + (wall_now - wall_anchor) x rate`. [`Pacer::step`]
/// takes the clock reading so the policy is testable without sleeping.
#[derive(Clone, Copy, Debug)]
pub struct Pacer {
    rate: f64,
    wall_anchor: Instant,
    vt_anchor: Duration,
}

/// The slowest accepted `Wall { rate }`: below it `Duration::div_f64` in [`Pacer::step`] panics.
pub const MIN_RATE: f64 = 1e-3;
/// The fastest accepted `Wall { rate }`: above it `Duration::mul_f64` in [`Pacer::target`]
/// overflows on a long run. Faster runs want `Max`, which does not pace.
pub const MAX_RATE: f64 = 1e3;

impl Pacer {
    /// A pacer anchored at `now`, with the guest at virtual time `vt`.
    ///
    /// `rate` 1.0 is real time. A rate outside [`MIN_RATE`]..=[`MAX_RATE`] (or not finite) is
    /// clamped to 1.0 rather than stalling the run or panicking in `Duration` arithmetic.
    pub fn new(rate: f64, now: Instant, vt: Duration) -> Pacer {
        Pacer {
            // `contains` also rejects NaN and infinity.
            rate: match (MIN_RATE..=MAX_RATE).contains(&rate) {
                true => rate,
                false => 1.0,
            },
            wall_anchor: now,
            vt_anchor: vt,
        }
    }

    pub fn rate(self) -> f64 {
        self.rate
    }

    /// The virtual time the guest should have reached by `now`.
    pub fn target(self, now: Instant) -> Duration {
        let elapsed = now.saturating_duration_since(self.wall_anchor);
        self.vt_anchor + elapsed.mul_f64(self.rate)
    }

    /// Moves both anchors to the guest's real position, forgetting the backlog.
    pub fn reanchor(&mut self, now: Instant, vt: Duration) {
        self.wall_anchor = now;
        self.vt_anchor = vt;
    }

    /// Decides what to do at a slice boundary where the guest reached `vt` and the host clock
    /// reads `now`. Does not sleep.
    pub fn step(&mut self, now: Instant, vt: Duration) -> Pace {
        let target = self.target(now);
        if let Some(ahead) = vt.checked_sub(target).filter(|a| !a.is_zero()) {
            return Pace::Sleep(ahead.div_f64(self.rate));
        }
        let behind = target - vt;
        if behind > MAX_LAG {
            self.reanchor(now, vt);
            return Pace::Reanchored { behind };
        }
        Pace::Run { behind }
    }

    /// [`Pacer::step`] against the host clock, sleeping when it says to. Returns the decision so
    /// the caller can report the real-time factor and journal a re-anchor.
    pub fn wait(&mut self, vt: Duration) -> Pace {
        let now = Instant::now();
        let pace = self.step(now, vt);
        if let Pace::Sleep(d) = pace {
            sleep_until(now + d);
        }
        pace
    }
}

/// How late the host woke this process over a run of identical waits. Lateness is the overshoot
/// past the deadline, never negative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lateness {
    pub samples: usize,
    pub requested: Duration,
    pub p50: Duration,
    pub p99: Duration,
    /// The worst single wake, which is what an audio underrun hears.
    pub max: Duration,
    /// Waits that returned before their deadline: always zero. Unlike the percentiles it does not
    /// depend on host load, so a default-run test can assert it.
    pub early: usize,
}

impl Lateness {
    fn percentile(sorted: &[Duration], percent: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        let rank = ((percent / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    }
}

/// Measures the wake lateness of `requested` waits for about `total` wall time, with no guest
/// running between waits, so the number is the host timer's alone. `xtask bench` records it.
pub fn measure_wake_lateness(requested: Duration, total: Duration) -> Lateness {
    let samples = match requested.is_zero() {
        true => 1,
        false => (total.as_nanos() / requested.as_nanos()).max(1) as usize,
    };
    let mut late = Vec::with_capacity(samples);
    let mut early = 0usize;
    for _ in 0..samples {
        let deadline = Instant::now() + requested;
        sleep_until(deadline);
        let woke = Instant::now();
        if woke < deadline {
            early += 1;
        }
        late.push(woke.saturating_duration_since(deadline));
    }
    late.sort_unstable();
    Lateness {
        samples: late.len(),
        requested,
        p50: Lateness::percentile(&late, 50.0),
        p99: Lateness::percentile(&late, 99.0),
        max: late.last().copied().unwrap_or(Duration::ZERO),
        early,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(1);

    /// Measures on a thread with the machine-thread class, because on Windows hybrid CPUs an
    /// unclassed background thread runs on the efficiency cores and its timer wake is theirs.
    fn measure_on_a_machine_thread(requested: Duration, total: Duration) -> (Lateness, String) {
        std::thread::spawn(move || {
            let class = crate::platform::machine_thread();
            (measure_wake_lateness(requested, total), class.to_owned())
        })
        .join()
        .expect("the measuring thread")
    }
    /// The band: p99 lateness at most 2 ms over 1 ms waits, on every supported OS.
    const BAND: Duration = Duration::from_millis(2);

    #[test]
    fn a_deadline_in_the_past_does_not_wait() {
        let start = Instant::now();
        sleep_until(start - Duration::from_secs(1));
        assert!(
            start.elapsed() < Duration::from_millis(1),
            "a past deadline returned immediately"
        );
    }

    #[test]
    fn a_wait_never_returns_before_its_deadline() {
        for wait in [Duration::from_micros(200), Duration::from_millis(3)] {
            let deadline = Instant::now() + wait;
            sleep_until(deadline);
            assert!(
                Instant::now() >= deadline,
                "slept past the deadline of a {wait:?} wait"
            );
        }
    }

    #[test]
    fn a_guest_ahead_of_wall_time_sleeps_the_difference() {
        let now = Instant::now();
        let mut pacer = Pacer::new(1.0, now, Duration::ZERO);
        assert_eq!(
            pacer.step(now, Duration::from_millis(8)),
            Pace::Sleep(Duration::from_millis(8))
        );
        let mut fast = Pacer::new(2.0, now, Duration::ZERO);
        assert_eq!(
            fast.step(now, Duration::from_millis(20)),
            Pace::Sleep(Duration::from_millis(10)),
            "at 2x, virtual time runs twice as fast as the wait that produces it"
        );
        let mut slow = Pacer::new(0.5, now, Duration::ZERO);
        assert_eq!(
            slow.step(now, Duration::from_millis(20)),
            Pace::Sleep(Duration::from_millis(40))
        );
    }

    #[test]
    fn the_target_follows_the_wall_clock_at_the_rate() {
        let now = Instant::now();
        let pacer = Pacer::new(0.25, now, Duration::from_millis(100));
        assert_eq!(pacer.target(now), Duration::from_millis(100));
        assert_eq!(
            pacer.target(now + Duration::from_millis(40)),
            Duration::from_millis(110)
        );
    }

    #[test]
    fn falling_behind_runs_and_falling_far_behind_re_anchors() {
        let start = Instant::now();
        let mut pacer = Pacer::new(1.0, start, Duration::ZERO);

        let a_little = start + Duration::from_millis(200);
        assert_eq!(
            pacer.step(a_little, Duration::from_millis(100)),
            Pace::Run {
                behind: Duration::from_millis(100)
            },
            "100 ms behind is under the 250 ms bound, so the guest just runs"
        );

        let a_lot = start + Duration::from_millis(600);
        let vt = Duration::from_millis(200);
        assert_eq!(
            pacer.step(a_lot, vt),
            Pace::Reanchored {
                behind: Duration::from_millis(400)
            }
        );
        assert_eq!(
            pacer.target(a_lot),
            vt,
            "the anchors moved to the guest, so it is no longer behind"
        );
        assert_eq!(
            pacer.step(a_lot, vt),
            Pace::Run {
                behind: Duration::ZERO
            },
            "and the backlog is gone rather than sprinted through"
        );
    }

    /// `1e-300` and `1e300` are finite and positive yet make `div_f64`/`mul_f64` panic, so the
    /// clamp is what keeps that panic out of a run.
    #[test]
    fn a_nonsense_rate_becomes_real_time() {
        let now = Instant::now();
        for rate in [
            0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            1e-300,
            1e300,
            MIN_RATE / 2.0,
            MAX_RATE * 2.0,
        ] {
            assert_eq!(
                Pacer::new(rate, now, Duration::ZERO).rate(),
                1.0,
                "a rate of {rate} is outside the band and becomes real time"
            );
        }
        for rate in [MIN_RATE, 0.5, 1.0, 2.0, MAX_RATE] {
            assert_eq!(Pacer::new(rate, now, Duration::ZERO).rate(), rate);
        }
        for rate in [MIN_RATE, MAX_RATE] {
            let mut pacer = Pacer::new(rate, now, Duration::ZERO);
            pacer.step(now, Duration::from_millis(1));
            pacer.target(now + Duration::from_secs(86_400));
        }
    }

    #[test]
    fn the_pacer_waits_only_when_the_guest_is_ahead() {
        let mut pacer = Pacer::new(1.0, Instant::now(), Duration::ZERO);
        let start = Instant::now();
        let pace = pacer.wait(Duration::from_millis(5));
        let slept = start.elapsed();
        assert!(matches!(pace, Pace::Sleep(_)), "{pace:?}");
        assert!(
            slept >= Duration::from_millis(4),
            "waited for the 5 ms of virtual time the guest was ahead, slept {slept:?}"
        );

        let start = Instant::now();
        let pace = pacer.wait(Duration::ZERO);
        assert!(matches!(pace, Pace::Run { .. }), "{pace:?}");
        assert!(
            start.elapsed() < Duration::from_millis(2),
            "a guest that is behind does not wait"
        );
    }

    #[test]
    fn the_percentile_is_nearest_rank() {
        let sorted: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(
            Lateness::percentile(&sorted, 50.0),
            Duration::from_millis(50)
        );
        assert_eq!(
            Lateness::percentile(&sorted, 99.0),
            Duration::from_millis(99)
        );
        assert_eq!(
            Lateness::percentile(&sorted, 100.0),
            Duration::from_millis(100)
        );
        assert_eq!(Lateness::percentile(&[], 99.0), Duration::ZERO);
    }

    /// Asserts only the load-independent invariants (sample count, ordering, no early return);
    /// on a loaded host the band itself fails, so `pacing_wake_lateness` asserts it.
    #[test]
    fn pacing_wake_lateness_short() {
        let (lateness, class) = measure_on_a_machine_thread(WAIT, Duration::from_millis(500));
        println!(
            "pacing_wake_lateness_short: {lateness:?} on a {class} thread (band {BAND:?}, not \
             asserted here)"
        );
        assert!(lateness.samples >= 500, "{lateness:?}");
        assert_eq!(
            lateness.early, 0,
            "`sleep_until` never returns before its deadline: {lateness:?}"
        );
        assert!(
            lateness.p50 <= lateness.p99 && lateness.p99 <= lateness.max,
            "the percentiles come from one sorted run: {lateness:?}"
        );
    }

    /// `#[ignore]`: 10 s is past the unit-test budget, and the band only means something on an
    /// idle host. Run with `cargo test -p pemu-host -- --ignored pacing_wake_lateness`.
    ///
    /// Measured on an i7-12700F under Windows 11 (debug build, idle, machine-class thread): p99
    /// 580 to 589 us, no early return. An unclassed thread on a loaded host read p99 2.99 ms.
    #[test]
    #[ignore = "10 s of measurement on an otherwise idle host, past the T0 budget; the band lives here alone"]
    fn pacing_wake_lateness() {
        let (lateness, class) = measure_on_a_machine_thread(WAIT, Duration::from_secs(10));
        println!("pacing_wake_lateness: {lateness:?} on a {class} thread");
        assert!(lateness.samples >= 10_000, "{lateness:?}");
        assert_eq!(lateness.early, 0, "{lateness:?}");
        assert!(
            lateness.p99 <= BAND,
            "pacing band: p99 lateness at most 2 ms over 10 s of 1 ms waits, \
             measured {lateness:?}"
        );
    }
}
