//! The receipt every command output carries: fidelity is reported, never implied. It names the
//! timing profile and CPI behind the virtual time, the eFuse source, taint, reproducibility,
//! fidelity classes, the first unmodeled registers touched and what the HLE layer synthesized.
//!
//! Two renderings: JSON ([`Receipt::to_json`]) in every `Output.json`, and one line
//! ([`Receipt::one_line`]), the text-mode suffix `fidelity: cpu B, display B, audio B, ble C |
//! profile fast | deterministic`.
//!
//! It never carries an absolute path: goldens compare receipts byte for byte across hosts. It is
//! not `pemu_machine::machine::Receipt`, the machine's ledger deltas, which the command layer folds
//! in so a ledger change never changes this response shape. Subsystems add fields through
//! [`Receipt::extra`]; unknown keys survive a JSON round trip.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::output::collect_absolute_paths;

/// The receipt spelling of `pemu_machine::config::EfuseSource`. The [`From`] impls below map the
/// two, so a new variant on either side is a build error rather than a wrong receipt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EfuseSource {
    /// Synthesized by the emulator, carrying no device identity. The default.
    #[default]
    Synthetic,
    /// Read from a device dump, which taints the machine.
    Dump,
}

impl EfuseSource {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            EfuseSource::Synthetic => "synthetic",
            EfuseSource::Dump => "dump",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<EfuseSource> {
        match name {
            "synthetic" => Some(EfuseSource::Synthetic),
            "dump" => Some(EfuseSource::Dump),
            _ => None,
        }
    }
}

impl From<pemu_machine::config::EfuseSource> for EfuseSource {
    fn from(source: pemu_machine::config::EfuseSource) -> EfuseSource {
        match source {
            pemu_machine::config::EfuseSource::Synth => EfuseSource::Synthetic,
            pemu_machine::config::EfuseSource::Dump => EfuseSource::Dump,
        }
    }
}

impl From<EfuseSource> for pemu_machine::config::EfuseSource {
    fn from(source: EfuseSource) -> pemu_machine::config::EfuseSource {
        match source {
            EfuseSource::Synthetic => pemu_machine::config::EfuseSource::Synth,
            EfuseSource::Dump => pemu_machine::config::EfuseSource::Dump,
        }
    }
}

/// How reproducible the run is: deterministic when no input source was worse. Folded over the
/// machine facade because `pemu-api` does not depend on `pemu-core`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Determinism {
    #[default]
    Deterministic,
    /// A nondeterministic bridge ran and its data is journaled in full, so a replay reproduces the
    /// run.
    Replayable,
    /// A live bridge ran without a complete journal: the run cannot be reproduced.
    Live,
}

impl Determinism {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Determinism::Deterministic => "deterministic",
            Determinism::Replayable => "replayable",
            Determinism::Live => "live",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<Determinism> {
        match name {
            "deterministic" => Some(Determinism::Deterministic),
            "replayable" => Some(Determinism::Replayable),
            "live" => Some(Determinism::Live),
            _ => None,
        }
    }
}

/// A fidelity class: A measured against silicon, B modeled from documentation, C approximated, U
/// unmodeled or unclaimed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FidelityClass {
    A,
    B,
    C,
    /// Unmodeled or unclaimed. The default, because an unlisted subsystem is not a claim.
    #[default]
    U,
}

impl FidelityClass {
    /// The one-character spelling used in receipts and `docs/fidelity.md`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            FidelityClass::A => "A",
            FidelityClass::B => "B",
            FidelityClass::C => "C",
            FidelityClass::U => "U",
        }
    }

    #[must_use]
    pub fn from_name(name: &str) -> Option<FidelityClass> {
        match name {
            "A" => Some(FidelityClass::A),
            "B" => Some(FidelityClass::B),
            "C" => Some(FidelityClass::C),
            "U" => Some(FidelityClass::U),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FidelityEntry {
    pub subsystem: String,
    pub class: FidelityClass,
}

/// Only C and U are listed: A and B are the claim, C and U the caveat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassesTouched {
    pub c: Vec<String>,
    pub u: Vec<String>,
}

impl ClassesTouched {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.c.is_empty() && self.u.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HleReceipt {
    /// For example `idf-5.5.3/ble`; `None` when nothing was bound.
    pub bound: Option<String>,
    /// Log lines the HLE layer synthesized rather than the guest printing them.
    pub synthesized_log_lines: u32,
    /// Guest code reached blob territory the HLE layer replaced.
    pub tripwires_hit: Vec<String>,
}

/// Whether unmodeled-hardware caveats fail the run. It changes only [`exit_code`]; the caveats are
/// the same either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Strictness {
    /// Agent default: caveats are reported, not failed on.
    #[default]
    Lenient,
    /// CI default: an unmodeled-hardware caveat exits 7 UNMODELED_HW.
    Strict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CaveatKind {
    /// Exits 7 under [`Strictness::Strict`].
    ClassU,
    /// A register with no class other than U, touched for the first time. Exits 7 under
    /// [`Strictness::Strict`] like [`CaveatKind::ClassU`].
    Unmodeled,
    Tripwire,
    TimingLint,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caveat {
    pub kind: CaveatKind,
    /// A register, a block, a tripwire or a lint.
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    PassWithCaveats,
    Fail,
}

impl Verdict {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::PassWithCaveats => "pass_with_caveats",
            Verdict::Fail => "fail",
        }
    }
}

/// 0 pass, 1 fail. Under `--strict` an unmodeled-hardware caveat exits 7 UNMODELED_HW and any other
/// `pass_with_caveats` exits 10; lenient never exits 7. The generated `docs/errors.md` table is
/// built from this rule.
#[must_use]
pub fn exit_code(verdict: Verdict, caveats: &[Caveat], strict: Strictness) -> u8 {
    match verdict {
        Verdict::Pass => 0,
        Verdict::Fail => 1,
        Verdict::PassWithCaveats => {
            let unmodeled = caveats
                .iter()
                .any(|c| matches!(c.kind, CaveatKind::ClassU | CaveatKind::Unmodeled));
            if strict == Strictness::Strict && unmodeled {
                7
            } else {
                10
            }
        }
    }
}

/// Hosts whose tests check the committed parity golden (`tests/golden/cross-host/parity.txt`). A
/// fact about the build, so every receipt carries it. Windows arm64 is only cross-checked, so it is
/// not named.
pub const HOST_PARITY: &str = "macos-aarch64,windows-x86_64";

#[derive(Clone, Debug, PartialEq)]
pub struct Receipt {
    /// Microseconds.
    pub vt_us: u64,
    pub insns: u64,
    /// `fast` or `device`.
    pub profile: String,
    /// Thousandths: 1000 is one cycle per instruction.
    pub cpi_milli: u32,
    pub efuse: EfuseSource,
    /// Whether a secret-bearing input loaded.
    pub tainted: bool,
    pub determinism: Determinism,
    /// Part of run identity.
    pub journal_len: u32,
    pub classes_touched: ClassesTouched,
    /// In first-touch order.
    pub unmodeled_first_touch: Vec<String>,
    pub hle: HleReceipt,
    pub timing_lint: Vec<String>,
    /// In one-line order; a list because a JSON object would sort it.
    pub fidelity: Vec<FidelityEntry>,
    /// A redacted snapshot restores with factory NVS and a synthetic cardid and says so here.
    pub redacted: bool,
    pub host_parity: Option<String>,
    /// Written at the top level of the JSON object, such as `rom` and `heap_fidelity`.
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for Receipt {
    /// Nothing run, nothing tainted, the `fast` profile at one cycle per instruction.
    fn default() -> Self {
        Receipt {
            vt_us: 0,
            insns: 0,
            profile: "fast".to_string(),
            cpi_milli: 1_000,
            efuse: EfuseSource::Synthetic,
            tainted: false,
            determinism: Determinism::Deterministic,
            journal_len: 0,
            classes_touched: ClassesTouched::default(),
            unmodeled_first_touch: Vec::new(),
            hle: HleReceipt::default(),
            timing_lint: Vec::new(),
            fidelity: Vec::new(),
            redacted: false,
            // `None` stays reachable through `from_json`, for a receipt from a build without the
            // parity word.
            host_parity: Some(HOST_PARITY.to_string()),
            extra: BTreeMap::new(),
        }
    }
}

/// [`Receipt::from_json`] puts every other key into [`Receipt::extra`].
const KNOWN_KEYS: &[&str] = &[
    "vt_us",
    "insns",
    "profile",
    "cpi_milli",
    "efuse",
    "tainted",
    "determinism",
    "journal_len",
    "classes_touched",
    "unmodeled_first_touch",
    "hle",
    "timing_lint",
    "fidelity",
    "redacted",
    "host_parity",
];

impl Receipt {
    /// [`Receipt::extra`] keys are written alongside the fixed ones.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert("vt_us".into(), self.vt_us.into());
        map.insert("insns".into(), self.insns.into());
        map.insert("profile".into(), self.profile.clone().into());
        map.insert("cpi_milli".into(), self.cpi_milli.into());
        map.insert("efuse".into(), self.efuse.name().into());
        map.insert("tainted".into(), self.tainted.into());
        map.insert("determinism".into(), self.determinism.name().into());
        map.insert("journal_len".into(), self.journal_len.into());
        map.insert(
            "classes_touched".into(),
            serde_json::json!({
                "C": self.classes_touched.c,
                "U": self.classes_touched.u,
            }),
        );
        map.insert(
            "unmodeled_first_touch".into(),
            self.unmodeled_first_touch.clone().into(),
        );
        map.insert(
            "hle".into(),
            serde_json::json!({
                "bound": self.hle.bound,
                "synthesized_log_lines": self.hle.synthesized_log_lines,
                "tripwires_hit": self.hle.tripwires_hit,
            }),
        );
        map.insert("timing_lint".into(), self.timing_lint.clone().into());
        map.insert(
            "fidelity".into(),
            serde_json::Value::Array(
                self.fidelity
                    .iter()
                    .map(|e| serde_json::json!({"subsystem": e.subsystem, "class": e.class.name()}))
                    .collect(),
            ),
        );
        map.insert("redacted".into(), self.redacted.into());
        map.insert("host_parity".into(), self.host_parity.clone().into());
        for (key, value) in &self.extra {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                map.insert(key.clone(), value.clone());
            }
        }
        serde_json::Value::Object(map)
    }

    /// Missing keys take their default and unknown keys land in [`Receipt::extra`], so an older
    /// build can read a newer receipt. `None` when the value is not an object.
    #[must_use]
    pub fn from_json(value: &serde_json::Value) -> Option<Receipt> {
        let map = value.as_object()?;
        let string = |key: &str| map.get(key).and_then(|v| v.as_str());
        let list = |key: &str| -> Vec<String> {
            map.get(key)
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let classes = map.get("classes_touched");
        let class_list = |key: &str| -> Vec<String> {
            classes
                .and_then(|v| v.get(key))
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let hle = map.get("hle");
        let mut receipt = Receipt {
            vt_us: map.get("vt_us").and_then(serde_json::Value::as_u64)?,
            insns: map
                .get("insns")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            profile: string("profile").unwrap_or("fast").to_string(),
            cpi_milli: u32::try_from(
                map.get("cpi_milli")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1_000),
            )
            .ok()?,
            efuse: string("efuse")
                .and_then(EfuseSource::from_name)
                .unwrap_or_default(),
            tainted: map
                .get("tainted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            determinism: string("determinism")
                .and_then(Determinism::from_name)
                .unwrap_or_default(),
            journal_len: u32::try_from(
                map.get("journal_len")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0),
            )
            .ok()?,
            classes_touched: ClassesTouched {
                c: class_list("C"),
                u: class_list("U"),
            },
            unmodeled_first_touch: list("unmodeled_first_touch"),
            hle: HleReceipt {
                bound: hle
                    .and_then(|v| v.get("bound"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                synthesized_log_lines: hle
                    .and_then(|v| v.get("synthesized_log_lines"))
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(0),
                tripwires_hit: hle
                    .and_then(|v| v.get("tripwires_hit"))
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            timing_lint: list("timing_lint"),
            fidelity: map
                .get("fidelity")
                .and_then(|v| v.as_array())
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            Some(FidelityEntry {
                                subsystem: item.get("subsystem")?.as_str()?.to_string(),
                                class: FidelityClass::from_name(item.get("class")?.as_str()?)?,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            redacted: map
                .get("redacted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            host_parity: string("host_parity").map(str::to_string),
            extra: BTreeMap::new(),
        };
        for (key, value) in map {
            if !KNOWN_KEYS.contains(&key.as_str()) {
                receipt.extra.insert(key.clone(), value.clone());
            }
        }
        Some(receipt)
    }

    /// The fidelity segment is omitted when no subsystem reported a class. `tainted` and `redacted`
    /// add their own segments, so an agent need not open the JSON to learn the output was masked.
    #[must_use]
    pub fn one_line(&self) -> String {
        let mut out = String::new();
        if !self.fidelity.is_empty() {
            out.push_str("fidelity: ");
            for (i, entry) in self.fidelity.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{} {}", entry.subsystem, entry.class.name());
            }
            out.push_str(" | ");
        }
        let _ = write!(
            out,
            "profile {} | {}",
            self.profile,
            self.determinism.name()
        );
        if self.tainted {
            out.push_str(" | tainted");
        }
        if self.redacted {
            out.push_str(" | redacted");
        }
        out
    }

    /// In a fixed order so a golden compares. Strictness does not belong here: a lenient run lists
    /// the same caveats, and `--strict` changes only [`exit_code`]. The caller has already removed
    /// allowlisted registers from `classes_touched.u`.
    #[must_use]
    pub fn caveats(&self) -> Vec<Caveat> {
        let mut out = Vec::new();
        for name in &self.classes_touched.u {
            out.push(Caveat {
                kind: CaveatKind::ClassU,
                detail: name.clone(),
            });
        }
        for name in &self.unmodeled_first_touch {
            out.push(Caveat {
                kind: CaveatKind::Unmodeled,
                detail: name.clone(),
            });
        }
        for name in &self.hle.tripwires_hit {
            out.push(Caveat {
                kind: CaveatKind::Tripwire,
                detail: name.clone(),
            });
        }
        for name in &self.timing_lint {
            out.push(Caveat {
                kind: CaveatKind::TimingLint,
                detail: name.clone(),
            });
        }
        out
    }

    /// `pass` only when the assertions hold and nothing was caveated, in both strictness modes.
    #[must_use]
    pub fn verdict(&self, assertions_hold: bool) -> Verdict {
        if !assertions_hold {
            Verdict::Fail
        } else if self.caveats().is_empty() {
            Verdict::Pass
        } else {
            Verdict::PassWithCaveats
        }
    }

    /// Must always be empty.
    #[must_use]
    pub fn absolute_paths(&self) -> Vec<String> {
        let mut found = Vec::new();
        collect_absolute_paths(&self.to_json(), &mut found);
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fidelity(pairs: &[(&str, FidelityClass)]) -> Vec<FidelityEntry> {
        pairs
            .iter()
            .map(|(name, class)| FidelityEntry {
                subsystem: (*name).to_string(),
                class: *class,
            })
            .collect()
    }

    fn arch_example() -> Receipt {
        Receipt {
            vt_us: 845_213,
            insns: 131_004_551,
            profile: "fast".to_string(),
            cpi_milli: 1_000,
            efuse: EfuseSource::Synthetic,
            tainted: false,
            determinism: Determinism::Deterministic,
            journal_len: 12,
            classes_touched: ClassesTouched {
                c: vec!["battery.ocv".to_string()],
                u: Vec::new(),
            },
            unmodeled_first_touch: Vec::new(),
            hle: HleReceipt {
                bound: Some("idf-5.5.3/ble".to_string()),
                synthesized_log_lines: 5,
                tripwires_hit: Vec::new(),
            },
            timing_lint: Vec::new(),
            fidelity: fidelity(&[
                ("cpu", FidelityClass::B),
                ("display", FidelityClass::B),
                ("audio", FidelityClass::B),
                ("ble", FidelityClass::C),
            ]),
            redacted: false,
            host_parity: Some("macos-only".to_string()),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn the_one_line_form_is_the_documented_string() {
        assert_eq!(
            arch_example().one_line(),
            "fidelity: cpu B, display B, audio B, ble C | profile fast | deterministic"
        );
    }

    #[test]
    fn the_one_line_form_says_tainted_and_redacted() {
        let mut receipt = arch_example();
        receipt.tainted = true;
        receipt.redacted = true;
        assert!(
            receipt
                .one_line()
                .ends_with("| deterministic | tainted | redacted")
        );
    }

    #[test]
    fn a_receipt_without_fidelity_entries_claims_nothing() {
        let receipt = Receipt::default();
        assert_eq!(receipt.one_line(), "profile fast | deterministic");
    }

    #[test]
    fn the_json_form_has_the_documented_fields() {
        let json = arch_example().to_json();
        assert_eq!(json["vt_us"], 845_213);
        assert_eq!(json["insns"], 131_004_551u64);
        assert_eq!(json["profile"], "fast");
        assert_eq!(json["cpi_milli"], 1_000);
        assert_eq!(json["efuse"], "synthetic");
        assert_eq!(json["tainted"], false);
        assert_eq!(json["determinism"], "deterministic");
        assert_eq!(json["journal_len"], 12);
        assert_eq!(
            json["classes_touched"]["C"],
            serde_json::json!(["battery.ocv"])
        );
        assert_eq!(json["classes_touched"]["U"], serde_json::json!([]));
        assert_eq!(json["unmodeled_first_touch"], serde_json::json!([]));
        assert_eq!(json["hle"]["bound"], "idf-5.5.3/ble");
        assert_eq!(json["hle"]["synthesized_log_lines"], 5);
        assert_eq!(json["hle"]["tripwires_hit"], serde_json::json!([]));
        assert_eq!(json["timing_lint"], serde_json::json!([]));
        assert_eq!(
            json["fidelity"],
            serde_json::json!([
                {"subsystem": "cpu", "class": "B"},
                {"subsystem": "display", "class": "B"},
                {"subsystem": "audio", "class": "B"},
                {"subsystem": "ble", "class": "C"},
            ])
        );
    }

    #[test]
    fn a_receipt_round_trips_through_json() {
        let receipt = arch_example();
        let back = Receipt::from_json(&receipt.to_json()).expect("an object parses");
        assert_eq!(back, receipt);
    }

    #[test]
    fn a_round_trip_keeps_the_fields_other_packages_added() {
        let mut receipt = arch_example();
        receipt
            .extra
            .insert("rom".to_string(), "unpinned".to_string().into());
        receipt.extra.insert(
            "heap_fidelity".to_string(),
            "blob_allocations_estimated".to_string().into(),
        );
        let json = receipt.to_json();
        assert_eq!(json["rom"], "unpinned");
        let back = Receipt::from_json(&json).expect("an object parses");
        assert_eq!(back, receipt);
        assert_eq!(back.extra["heap_fidelity"], "blob_allocations_estimated");
    }

    #[test]
    fn a_receipt_round_trips_and_carries_no_absolute_path() {
        let mut receipt = arch_example();
        receipt.tainted = true;
        receipt.redacted = true;
        receipt.extra.insert(
            "artifacts".to_string(),
            "20260911-0612-a1/p1/serial.log".to_string().into(),
        );
        let back = Receipt::from_json(&receipt.to_json()).expect("an object parses");
        assert_eq!(back, receipt);
        assert!(
            back.absolute_paths().is_empty(),
            "a relative, forward-slashed artifact path: {:?}",
            back.absolute_paths()
        );
    }

    #[test]
    fn the_absolute_path_guard_catches_a_native_root_on_either_host() {
        for root in [
            "/var/lib/passportsim/artifacts",
            "d:\\passportsim\\artifacts",
        ] {
            let mut receipt = Receipt::default();
            receipt
                .extra
                .insert("artifacts_root".to_string(), root.to_string().into());
            assert_eq!(receipt.absolute_paths(), vec![root.to_string()]);
        }
    }

    #[test]
    fn a_home_redacted_root_is_not_an_absolute_path() {
        let mut receipt = Receipt::default();
        receipt.extra.insert(
            "artifacts_root".to_string(),
            "~/.local/share/passportsim/artifacts".to_string().into(),
        );
        assert!(receipt.absolute_paths().is_empty());
    }

    #[test]
    fn an_older_build_reads_a_newer_receipt() {
        let json = serde_json::json!({"vt_us": 7, "profile": "device", "a_field_from_2027": 42});
        let receipt = Receipt::from_json(&json).expect("an object parses");
        assert_eq!(receipt.vt_us, 7);
        assert_eq!(receipt.profile, "device");
        assert_eq!(receipt.cpi_milli, 1_000);
        assert_eq!(receipt.extra["a_field_from_2027"], 42);
    }

    #[test]
    fn a_non_object_is_not_a_receipt() {
        assert!(Receipt::from_json(&serde_json::json!([1, 2])).is_none());
    }

    #[test]
    fn a_clean_run_passes_under_both_strictness_settings() {
        let receipt = arch_example();
        assert_eq!(receipt.verdict(true), Verdict::Pass);
        for strict in [Strictness::Lenient, Strictness::Strict] {
            assert_eq!(exit_code(Verdict::Pass, &[], strict), 0);
        }
    }

    #[test]
    fn a_class_u_touch_is_a_caveat_in_both_modes_and_exits_7_under_strict() {
        let mut receipt = arch_example();
        receipt.classes_touched.u = vec!["rmt.conf0".to_string()];
        let caveats = receipt.caveats();
        assert_eq!(caveats.len(), 1);
        assert_eq!(caveats[0].kind, CaveatKind::ClassU);
        assert_eq!(caveats[0].detail, "rmt.conf0");
        let verdict = receipt.verdict(true);
        assert_eq!(verdict, Verdict::PassWithCaveats);
        assert_eq!(exit_code(verdict, &caveats, Strictness::Strict), 7);
        assert_eq!(exit_code(verdict, &caveats, Strictness::Lenient), 10);
    }

    #[test]
    fn an_unmodeled_first_touch_is_a_lenient_caveat_and_exits_7_under_strict() {
        let mut receipt = arch_example();
        receipt.unmodeled_first_touch = vec!["uart1.reserved_0x40".to_string()];
        let caveats = receipt.caveats();
        assert_eq!(caveats.len(), 1);
        assert_eq!(caveats[0].kind, CaveatKind::Unmodeled);
        assert_eq!(caveats[0].detail, "uart1.reserved_0x40");
        let verdict = receipt.verdict(true);
        assert_eq!(verdict, Verdict::PassWithCaveats);
        assert_eq!(verdict.name(), "pass_with_caveats");
        assert_eq!(exit_code(verdict, &caveats, Strictness::Strict), 7);
        assert_eq!(exit_code(verdict, &caveats, Strictness::Lenient), 10);
    }

    #[test]
    fn any_other_caveat_exits_10() {
        let mut receipt = arch_example();
        receipt.timing_lint = vec!["spi2.trans_done at now".to_string()];
        let caveats = receipt.caveats();
        assert_eq!(caveats.len(), 1);
        assert_eq!(caveats[0].kind, CaveatKind::TimingLint);
        let verdict = receipt.verdict(true);
        assert_eq!(verdict, Verdict::PassWithCaveats);
        for strict in [Strictness::Lenient, Strictness::Strict] {
            assert_eq!(exit_code(verdict, &caveats, strict), 10);
        }
        assert_eq!(verdict.name(), "pass_with_caveats");
    }

    #[test]
    fn a_failed_assertion_fails_whatever_the_caveats() {
        let receipt = arch_example();
        assert_eq!(receipt.verdict(false), Verdict::Fail);
        assert_eq!(exit_code(Verdict::Fail, &[], Strictness::Strict), 1);
    }

    #[test]
    fn a_tripwire_is_a_caveat_in_both_modes() {
        let mut receipt = arch_example();
        receipt.hle.tripwires_hit = vec!["r_lld_pdu_rx_handler".to_string()];
        let caveats = receipt.caveats();
        assert!(caveats.iter().any(|c| c.kind == CaveatKind::Tripwire));
        assert_eq!(receipt.verdict(true), Verdict::PassWithCaveats);
        for strict in [Strictness::Lenient, Strictness::Strict] {
            assert_eq!(exit_code(Verdict::PassWithCaveats, &caveats, strict), 10);
        }
    }

    #[test]
    fn the_efuse_source_converts_to_and_from_the_machine_config_enum() {
        for (api, config) in [
            (
                EfuseSource::Synthetic,
                pemu_machine::config::EfuseSource::Synth,
            ),
            (EfuseSource::Dump, pemu_machine::config::EfuseSource::Dump),
        ] {
            assert_eq!(EfuseSource::from(config), api);
            assert_eq!(pemu_machine::config::EfuseSource::from(api), config);
        }
    }
}
