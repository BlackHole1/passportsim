//! Call-trace diff: the order of entries into the named functions of a boot phase, from our
//! observe hooks against QEMU gdb breakpoints.
//!
//! - **Only the order matters.** A gdb breakpoint gives no time and no instruction count.
//! - **The watched set is an input**, from the oracle run's configuration;
//!   [`CallTrace::restrict`] narrows both sides to the same set so a breakpoint the run forgot
//!   cannot look like a missing call.
//!
//! The alignment is the write streams' LCS ([`crate::lcs::align`]), so an inserted or missing
//! call is reported as such, and it is windowed like [`crate::lcs::diff_block`]: one entry per
//! **call**, so a phase can enter the watched set tens of thousands of times.

use std::collections::BTreeSet;

use crate::lcs::{CONTEXT, Edit, WINDOW, align};

/// The boot-phase functions named outright. A phase's full watched set (about 60) is supplied per
/// run, since the BSP init functions depend on the image; this is a seed.
pub const NAMED_BOOT_FUNCTIONS: &[&str] = &[
    "bootloader_init",
    "esp_image_load",
    "call_start_cpu0",
    "heap_caps_init",
    "esp_flash_init",
    "app_main",
];

/// One entry into a watched function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub index: usize,
    pub function: String,
}

/// An ordered call trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CallTrace {
    /// Where the trace came from, for reports: `ours`, `qemu-gdb`, a probe name.
    pub source: String,
    pub entries: Vec<Entry>,
}

impl CallTrace {
    /// Builds a trace from an ordered list of function names.
    pub fn from_names<I, S>(source: impl Into<String>, names: I) -> CallTrace
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        CallTrace {
            source: source.into(),
            entries: names
                .into_iter()
                .enumerate()
                .map(|(index, function)| Entry {
                    index,
                    function: function.into(),
                })
                .collect(),
        }
    }

    /// Parses `CALL <function>` lines, anything else ignored: the format our gdb driver
    /// (`tools/oracle/trace_calls.py`) and our observe-hook dump both write, so the oracle is only
    /// ever read through its output.
    pub fn parse(source: impl Into<String>, text: &str) -> CallTrace {
        let names = text
            .replace('\r', "")
            .split('\n')
            .filter_map(|line| line.trim().strip_prefix("CALL ").map(str::trim))
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        CallTrace::from_names(source, names)
    }

    /// The trace narrowed to a watched set, renumbered from zero.
    pub fn restrict(&self, watched: &BTreeSet<String>) -> CallTrace {
        CallTrace::from_names(
            self.source.clone(),
            self.entries
                .iter()
                .filter(|entry| watched.contains(&entry.function))
                .map(|entry| entry.function.clone()),
        )
    }

    pub fn names(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.function.as_str())
            .collect()
    }
}

/// What the first call-order divergence looks like.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// We entered a function the oracle did not, at this point.
    OnlyOurs(Entry),
    /// The oracle entered a function we did not, at this point.
    OnlyOracle(Entry),
    /// Both entered a function here and the names differ.
    Reordered {
        ours: Entry,
        /// The oracle's.
        oracle: Entry,
    },
}

/// The result of a call-trace comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallDiff {
    /// Entries that aligned before the first divergence.
    pub aligned: usize,
    pub ours_len: usize,
    pub oracle_len: usize,
    pub first: Option<Divergence>,
    /// Up to [`CONTEXT`] names before it, oldest first.
    pub context: Vec<String>,
}

impl CallDiff {
    /// True when the two traces enter the same functions in the same order.
    pub fn is_equal(&self) -> bool {
        self.first.is_none()
    }
}

/// Compares the order of entries of two traces: walked forward while they agree, the first
/// disagreement classified by an LCS over [`WINDOW`] entries.
pub fn diff(ours: &CallTrace, oracle: &CallTrace) -> CallDiff {
    diff_with(ours, oracle, &mut |_| false)
}

/// [`diff`], skipping the divergences `excused` accepts: an excused one-sided entry advances that
/// side, an excused reordering both, and the walk goes on, so a listed extra call cannot hide an
/// unlisted one after it. Each step advances at least one side, so it terminates.
pub fn diff_with(
    ours: &CallTrace,
    oracle: &CallTrace,
    excused: &mut dyn FnMut(&Divergence) -> bool,
) -> CallDiff {
    let (our_names, oracle_names) = (ours.names(), oracle.names());
    let (mut i, mut j, mut aligned) = (0, 0, 0);
    let first = loop {
        while i < our_names.len() && j < oracle_names.len() && our_names[i] == oracle_names[j] {
            i += 1;
            j += 1;
            aligned += 1;
        }
        let Some(kind) = classify(&our_names[i..], &oracle_names[j..]) else {
            break None;
        };
        let divergence = match kind {
            Classified::OnlyOurs => Divergence::OnlyOurs(ours.entries[i].clone()),
            Classified::OnlyOracle => Divergence::OnlyOracle(oracle.entries[j].clone()),
            Classified::Reordered => Divergence::Reordered {
                ours: ours.entries[i].clone(),
                oracle: oracle.entries[j].clone(),
            },
        };
        if !excused(&divergence) {
            break Some(divergence);
        }
        match kind {
            Classified::OnlyOurs => i += 1,
            Classified::OnlyOracle => j += 1,
            Classified::Reordered => {
                i += 1;
                j += 1;
            }
        }
    };
    CallDiff {
        aligned,
        ours_len: ours.entries.len(),
        oracle_len: oracle.entries.len(),
        first,
        context: our_names[i.saturating_sub(CONTEXT)..i]
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    }
}

/// What the head of two disagreeing traces looks like, before the entries are attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Classified {
    /// Our entry is the extra one.
    OnlyOurs,
    /// The oracle's entry is the extra one.
    OnlyOracle,
    /// Both entered a function here and the names differ.
    Reordered,
}

/// Classifies the disagreement at the head of two name streams, looking no further than
/// [`WINDOW`] entries on each side.
fn classify(ours: &[&str], oracle: &[&str]) -> Option<Classified> {
    match (ours.first(), oracle.first()) {
        (None, None) => return None,
        (Some(_), None) => return Some(Classified::OnlyOurs),
        (None, Some(_)) => return Some(Classified::OnlyOracle),
        (Some(_), Some(_)) => {}
    }
    let script = align(
        &ours[..ours.len().min(WINDOW)],
        &oracle[..oracle.len().min(WINDOW)],
    );
    let mut edits = script.iter();
    Some(match (edits.next(), edits.next()) {
        (Some(Edit::OnlyOurs(_)), Some(Edit::OnlyOracle(_)))
        | (Some(Edit::OnlyOracle(_)), Some(Edit::OnlyOurs(_))) => Classified::Reordered,
        (Some(Edit::OnlyOracle(_)), _) => Classified::OnlyOracle,
        // The two heads differ, so the script cannot open with `Equal`; every remaining shape
        // starts with a call only we made.
        _ => Classified::OnlyOurs,
    })
}

/// Renders a call-trace comparison the way a failing oracle test prints it.
pub fn render(ours: &CallTrace, oracle: &CallTrace, diff: &CallDiff) -> String {
    let mut out = format!(
        "call trace {} against {}: {} of ours, {} of the oracle's, {} aligned\n",
        ours.source, oracle.source, diff.ours_len, diff.oracle_len, diff.aligned
    );
    for name in &diff.context {
        out.push_str(&format!("  both   {name}\n"));
    }
    match &diff.first {
        None => out.push_str("  no divergence\n"),
        Some(Divergence::OnlyOurs(entry)) => out.push_str(&format!(
            "  first divergence: only ours [{}] {}\n",
            entry.index, entry.function
        )),
        Some(Divergence::OnlyOracle(entry)) => out.push_str(&format!(
            "  first divergence: only the oracle [{}] {}\n",
            entry.index, entry.function
        )),
        Some(Divergence::Reordered { ours, oracle }) => out.push_str(&format!(
            "  first divergence: ours [{}] {} against the oracle [{}] {}\n",
            ours.index, ours.function, oracle.index, oracle.function
        )),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watched(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    fn boot(names: &[&str]) -> CallTrace {
        CallTrace::from_names("ours", names.iter().copied())
    }

    #[test]
    fn the_named_boot_functions_are_the_ones_arch_lists() {
        assert!(NAMED_BOOT_FUNCTIONS.contains(&"call_start_cpu0"));
        assert!(NAMED_BOOT_FUNCTIONS.contains(&"app_main"));
        assert_eq!(NAMED_BOOT_FUNCTIONS.len(), 6);
    }

    #[test]
    fn parses_the_gdb_driver_line_format() {
        let trace = CallTrace::parse(
            "qemu-gdb",
            "# probe run\nCALL bootloader_init\nBreakpoint 3 hit\r\nCALL esp_image_load\nCALL  app_main \n",
        );
        assert_eq!(
            trace.names(),
            vec!["bootloader_init", "esp_image_load", "app_main"]
        );
        assert_eq!(trace.entries[2].index, 2);
        assert_eq!(trace.source, "qemu-gdb");
    }

    #[test]
    fn identical_orders_do_not_diverge() {
        let ours = boot(&["bootloader_init", "esp_image_load", "call_start_cpu0"]);
        let oracle = CallTrace::from_names("qemu-gdb", ours.names());
        let diff = diff(&ours, &oracle);
        assert!(diff.is_equal());
        assert_eq!(diff.aligned, 3);
        assert!(render(&ours, &oracle, &diff).contains("no divergence"));
    }

    #[test]
    fn a_call_only_we_make_is_reported() {
        let ours = boot(&["bootloader_init", "esp_flash_init", "esp_image_load"]);
        let oracle = CallTrace::from_names("qemu-gdb", ["bootloader_init", "esp_image_load"]);
        let diff = diff(&ours, &oracle);
        assert_eq!(diff.aligned, 1);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs(Entry {
                index: 1,
                function: "esp_flash_init".to_string()
            }))
        );
    }

    #[test]
    fn a_call_only_the_oracle_makes_is_reported() {
        let ours = boot(&["bootloader_init", "esp_image_load"]);
        let oracle = CallTrace::from_names(
            "qemu-gdb",
            ["bootloader_init", "esp_flash_init", "esp_image_load"],
        );
        let diff = diff(&ours, &oracle);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOracle(Entry {
                index: 1,
                function: "esp_flash_init".to_string()
            }))
        );
    }

    #[test]
    fn two_functions_called_in_the_other_order_point_at_the_one_that_moved() {
        // A swap is an insertion plus a deletion under an LCS, and the report names the call
        // that moved forward rather than claiming both sides did something different.
        let ours = boot(&["heap_caps_init", "esp_flash_init", "app_main"]);
        let oracle =
            CallTrace::from_names("qemu-gdb", ["esp_flash_init", "heap_caps_init", "app_main"]);
        let diff = diff(&ours, &oracle);
        assert_eq!(diff.aligned, 0);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs(Entry {
                index: 0,
                function: "heap_caps_init".to_string()
            }))
        );
    }

    #[test]
    fn a_different_function_at_the_same_point_is_reported_as_reordered() {
        let ours = boot(&["bootloader_init", "heap_caps_init"]);
        let oracle = CallTrace::from_names("qemu-gdb", ["bootloader_init", "esp_flash_init"]);
        let diff = diff(&ours, &oracle);
        let Some(Divergence::Reordered { ours, oracle }) = diff.first else {
            panic!("{diff:?}");
        };
        assert_eq!(ours.function, "heap_caps_init");
        assert_eq!(oracle.function, "esp_flash_init");
    }

    #[test]
    fn restricting_to_the_watched_set_hides_an_unwatched_call() {
        let ours = boot(&["bootloader_init", "bsp_i2c_init", "app_main"]);
        let oracle = CallTrace::from_names("qemu-gdb", ["bootloader_init", "app_main"]);
        let set = watched(&["bootloader_init", "app_main"]);
        assert!(diff(&ours.restrict(&set), &oracle.restrict(&set)).is_equal());
        assert!(!diff(&ours, &oracle).is_equal());
    }

    #[test]
    fn a_long_phase_is_compared_without_a_full_table() {
        // A phase entering the watched set 20,000 times each side would need a 20001 x 20001 u32
        // table (about 1.6 GB) unwindowed. The walk must stay bounded and still point at the call
        // that moved.
        let long: Vec<String> = (0..20_000)
            .map(|i| NAMED_BOOT_FUNCTIONS[i % NAMED_BOOT_FUNCTIONS.len()].to_string())
            .collect();
        let ours = CallTrace::from_names("ours", long.clone());
        let mut theirs = long.clone();
        theirs.remove(12_345);
        let oracle = CallTrace::from_names("qemu-gdb", theirs);
        let diff = diff(&ours, &oracle);
        assert_eq!(diff.aligned, 12_345);
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs(ours.entries[12_345].clone()))
        );
        assert_eq!(diff.ours_len, 20_000);
        assert_eq!(diff.oracle_len, 19_999);
        // Two long traces that agree end to end are equal, and cost the same walk.
        let same = CallTrace::from_names("qemu-gdb", long);
        assert!(super::diff(&ours, &same).is_equal());
    }

    #[test]
    fn the_report_carries_context_before_the_divergence() {
        let mut names: Vec<String> = (0..30).map(|i| format!("f{i}")).collect();
        let ours = CallTrace::from_names("ours", names.clone());
        names.remove(25);
        let oracle = CallTrace::from_names("qemu-gdb", names);
        let diff = diff(&ours, &oracle);
        assert_eq!(diff.aligned, 25);
        assert_eq!(diff.context.len(), CONTEXT);
        assert_eq!(diff.context[0], "f5");
        assert_eq!(
            diff.first,
            Some(Divergence::OnlyOurs(Entry {
                index: 25,
                function: "f25".to_string()
            }))
        );
    }
}
