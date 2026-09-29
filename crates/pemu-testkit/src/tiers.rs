//! Tier naming and the nextest filter sets of the test layers.
//!
//! - **Milestone tests** are named `t<tier>_m<milestone>_<slug>` and live only in
//!   `tests/milestones/m<N>.rs`.
//! - **Layers L0 to L7**: [`Layer`] carries where each lives and which tier runs it, and
//!   [`Layer::filter`] and [`tier_filter`] render it as a nextest filter, so CI cannot drift.
//!
//! nextest matches `test(/<re>/)` against the *module-qualified* name, so filters anchor on
//! `(^|::)`, not `^`: otherwise a `t1_` test inside a `mod` would land in the T0 set.

use core::fmt;

/// Regex fragment that anchors a test-name prefix at the start of the last `::` segment.
pub const SEGMENT: &str = "(^|::)";

/// CI tier: T0 on every commit, T1 on every merge, T2 nightly.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Every commit and every work-package branch; needs no corpus and no device data.
    T0,
    /// Every merge to `main`; needs the corpus and the goldens, macOS-only.
    T1,
    /// Nightly and on demand; oracle diffs, long scenarios, browsers and benchmarks.
    T2,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::T0, Tier::T1, Tier::T2];

    /// The tier name as it is written in `cargo xtask ci <tier>` and in a receipt.
    pub fn name(self) -> &'static str {
        match self {
            Tier::T0 => "t0",
            Tier::T1 => "t1",
            Tier::T2 => "t2",
        }
    }

    /// The test-name prefix of this tier: `t0_`, `t1_` or `t2_`.
    pub fn prefix(self) -> &'static str {
        match self {
            Tier::T0 => "t0_",
            Tier::T1 => "t1_",
            Tier::T2 => "t2_",
        }
    }

    pub fn of_test(name: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| name.starts_with(t.prefix()))
    }

    /// Nextest filter for the tests **named** for this tier: `test(/(^|::)t1_/)`. Only milestone
    /// tests and the scenario tests beside them carry a prefix, never a crate's unit tests.
    pub fn name_filter(self) -> String {
        format!("test(/{SEGMENT}{}/)", self.prefix())
    }

    /// Nextest filterset of everything this tier runs. T1 and T2 run exactly the tests named for
    /// them; T0 runs everything else, since only corpus-, golden- and oracle-bound tests name
    /// themselves `t1_` or `t2_`.
    pub fn filter(self) -> String {
        match self {
            Tier::T0 => format!(
                "all() - {} - {}",
                Tier::T1.name_filter(),
                Tier::T2.name_filter()
            ),
            Tier::T1 | Tier::T2 => self.name_filter(),
        }
    }

    /// Nextest filter for the tests of one milestone at this tier: `test(/(^|::)t1_m3_/)`.
    ///
    /// `milestone` is spelled as in the test name, digits plus an optional `a`/`b` (`"3"`,
    /// `"9a"`); `M9a` and `M9b` are separate sets, so `milestone_filter("9")` selects neither.
    ///
    /// # Panics
    ///
    /// When `milestone` is not such an id.
    #[track_caller]
    pub fn milestone_filter(self, milestone: &str) -> String {
        let milestone = checked_milestone(milestone);
        format!("test(/{SEGMENT}{}m{milestone}_/)", self.prefix())
    }

    /// Nextest filter for every tier's tests of one milestone: `test(/(^|::)t[012]_m3_/)`.
    ///
    /// # Panics
    ///
    /// When `milestone` is not a milestone id (see [`Tier::milestone_filter`]).
    #[track_caller]
    pub fn every_tier_milestone_filter(milestone: &str) -> String {
        let milestone = checked_milestone(milestone);
        format!("test(/{SEGMENT}t[012]_m{milestone}_/)")
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Leading milestone id of `text` (digits plus an optional `a` or `b`) plus the rest. `None` when
/// there is no digit.
fn split_milestone(text: &str) -> Option<(&str, &str)> {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let end = match text.as_bytes().get(digits) {
        Some(b'a' | b'b') => digits + 1,
        _ => digits,
    };
    Some((&text[..end], &text[end..]))
}

#[track_caller]
fn checked_milestone(milestone: &str) -> &str {
    match split_milestone(milestone) {
        Some((id, "")) => id,
        _ => panic!("`{milestone}` is not a milestone id: digits with an optional `a` or `b`"),
    }
}

/// One test layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// L0: unit, generated register and property tests, in each crate.
    L0,
    /// L1: model harness tests, `RegHarness` plus `MockBoard`, busy-wait rows, chip datasheet
    /// sequences and I2C transcripts.
    L1,
    /// L2: CPU conformance, riscv-tests, CSR tests, the objdump decode corpus and the fuzz.
    L2,
    /// L3: golden boots against device lines and oracle consoles.
    L3,
    /// L4: scenarios.
    L4,
    /// L5: oracle diffs, macOS-only.
    L5,
    /// L6: determinism and self-consistency.
    L6,
    /// L7: browser, wasm parity and benchmarks.
    L7,
}

impl Layer {
    pub const ALL: [Layer; 8] = [
        Layer::L0,
        Layer::L1,
        Layer::L2,
        Layer::L3,
        Layer::L4,
        Layer::L5,
        Layer::L6,
        Layer::L7,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Layer::L0 => "L0",
            Layer::L1 => "L1",
            Layer::L2 => "L2",
            Layer::L3 => "L3",
            Layer::L4 => "L4",
            Layer::L5 => "L5",
            Layer::L6 => "L6",
            Layer::L7 => "L7",
        }
    }

    /// One phrase for what the layer tests.
    pub fn what(self) -> &'static str {
        match self {
            Layer::L0 => "unit, generated register and property tests",
            Layer::L1 => {
                "model harness tests: RegHarness plus MockBoard, busy-wait rows, chips, I2C transcripts"
            }
            Layer::L2 => "CPU conformance: riscv-tests, CSR tests, decode corpus, reference fuzz",
            Layer::L3 => "golden boots against device lines and oracle consoles",
            Layer::L4 => "scenarios",
            Layer::L5 => "oracle diffs: write-stream LCS, call trace, instruction counts",
            Layer::L6 => "determinism and self-consistency",
            Layer::L7 => "browser and wasm parity per host, benchmarks",
        }
    }

    /// The workspace packages the layer's Rust tests live in. L0 is empty because it is every
    /// crate, so its filter is the tier filter alone. L7 is empty because its work is not a
    /// `cargo test` target (Playwright and `xtask bench`). L5's comparison code is tested in
    /// `pemu-verify`.
    pub fn packages(self) -> &'static [&'static str] {
        match self {
            Layer::L0 => &[],
            Layer::L1 => &["pemu-soc-c3", "pemu-board"],
            Layer::L2 => &["pemu-rv32"],
            Layer::L3 => &["pemu-milestones", "pemu-testkit"],
            Layer::L4 => &["pemu-milestones"],
            Layer::L5 => &["pemu-verify"],
            Layer::L6 => &["pemu-machine", "pemu-testkit"],
            Layer::L7 => &[],
        }
    }

    /// The tiers that run this layer; which tests belong to which tier is their `t<tier>_`
    /// prefix.
    pub fn tiers(self) -> &'static [Tier] {
        match self {
            Layer::L0 | Layer::L1 => &[Tier::T0],
            Layer::L2 => &[Tier::T0, Tier::T1],
            Layer::L3 => &[Tier::T1],
            Layer::L4 | Layer::L6 => &[Tier::T1, Tier::T2],
            Layer::L5 => &[Tier::T2],
            Layer::L7 => &[Tier::T1, Tier::T2],
        }
    }

    pub fn runs_in(self, tier: Tier) -> bool {
        self.tiers().contains(&tier)
    }

    /// The nextest filterset of this layer at `tier`: `(package(a) + package(b)) & <tier filter>`,
    /// the tier filter alone for L0, and `none()` for a layer with no cargo target or not run at
    /// `tier`.
    pub fn filter(self, tier: Tier) -> String {
        if !self.runs_in(tier) {
            return "none()".to_string();
        }
        match self.packages() {
            [] if self == Layer::L0 => tier.filter(),
            [] => "none()".to_string(),
            packages => {
                let names: Vec<String> = packages.iter().map(|p| format!("package({p})")).collect();
                format!("({}) & ({})", names.join(" + "), tier.filter())
            }
        }
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// The nextest filterset of every layer `tier` runs; layers with no cargo test target drop out,
/// because `none()` unions away.
pub fn tier_filter(tier: Tier) -> String {
    let parts: Vec<String> = Layer::ALL
        .into_iter()
        .map(|layer| layer.filter(tier))
        .filter(|f| f != "none()")
        .collect();
    if parts.is_empty() {
        return "none()".to_string();
    }
    parts.join(" + ")
}
