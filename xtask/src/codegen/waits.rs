//! The `busy-waits` logical table, merged out of the `[[wait]]` rows of every
//! `specs/blocks/<block>.toml` into `crates/pemu-soc-c3/src/gen/waits.rs`.
//!
//! The other two merged tables (`reset-domains`, `fidelity-overrides`) reach the `RegSpec` rows
//! through `blocks::apply`; a busy-wait row belongs to no register column (trigger, expectation,
//! bound per timing profile, polling functions and images), so it gets its own table. Its two
//! consumers are the block packages' model tests and the hang detector.
//! The table is data only: it restates the block files' rows after the shape checks of
//! `blocks.rs` (unique ids, [`super::blocks::SEED_WAITS`] seed rows, register and field of the
//! block).

use std::fmt::Write as _;

use super::blocks::{Spec, Within};

/// Generated file, relative to the workspace root.
pub const OUTPUT_PATH: &str = "crates/pemu-soc-c3/src/gen/waits.rs";

/// Module name of the generated table inside `gen/mod.rs`.
pub const MODULE: &str = "waits";

/// Renders the whole table out of the parsed block files, in file-name then row order.
pub fn render(specs: &[Spec]) -> String {
    let rows: Vec<(&Spec, &super::blocks::WaitRow)> = specs
        .iter()
        .flat_map(|s| s.waits.iter().map(move |w| (s, w)))
        .collect();
    let seeds = rows.iter().filter(|(_, w)| w.seed).count();
    let mut out = String::new();
    let _ = writeln!(out, "{}", super::GENERATED_HEADER);
    out.push_str(DOC);
    out.push_str(TYPES);
    let _ = writeln!(
        out,
        "/// Number of busy-wait rows the block files carry.\n\
         pub const WAIT_COUNT: usize = {};\n",
        rows.len()
    );
    let _ = writeln!(
        out,
        "/// Rows with `seed = true`: the seed set.\n\
         pub const SEED_COUNT: usize = {seeds};\n"
    );
    out.push_str(
        "/// Every `[[wait]]` row of `specs/blocks/*.toml`, in file-name then row order.\n\
         pub static WAITS: [WaitSpec; WAIT_COUNT] = [\n",
    );
    for (spec, w) in &rows {
        out.push_str(&row(spec, w));
    }
    out.push_str("];\n");
    out.push_str(LOOKUPS);
    out.push_str(TESTS);
    out
}

/// One `WaitSpec` initializer.
fn row(spec: &Spec, w: &super::blocks::WaitRow) -> String {
    let kind = if w.kind == "tripwire" {
        "WaitKind::Tripwire"
    } else {
        "WaitKind::Wait"
    };
    let within = match &w.within {
        Within::All(bound) => format!("Within::All({})", quote(bound)),
        Within::PerProfile { fast, device } => format!(
            "Within::PerProfile {{ fast: {}, device: {} }}",
            quote(fast),
            quote(device)
        ),
    };
    let mut out = String::new();
    let _ = write!(
        out,
        "    WaitSpec {{\n\
         \x20       id: {},\n\
         \x20       kind: {kind},\n\
         \x20       seed: {},\n\
         \x20       block: {},\n\
         \x20       register: {},\n\
         \x20       field: {},\n\
         \x20       trigger: {},\n\
         \x20       expect: {},\n\
         \x20       within: {within},\n\
         \x20       polled_at: &[{}],\n\
         \x20       images: &[{}],\n\
         \x20       milestone: {},\n\
         \x20   }},\n",
        quote(&w.id),
        w.seed,
        quote(&spec.name),
        quote(&w.register),
        quote(&w.field),
        quote(&w.trigger),
        quote(&w.expect),
        list(&w.polled_at),
        list(&w.images),
        quote(&w.milestone),
    );
    out
}

/// A comma-separated list of string literals.
fn list(items: &[String]) -> String {
    items
        .iter()
        .map(|i| quote(i))
        .collect::<Vec<String>>()
        .join(", ")
}

/// A Rust string literal: the spec files hold prose, so escape the two characters that would end
/// or continue the literal.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

const DOC: &str = r#"//!
//! The `busy-waits` table, one row per `[[wait]]` row of `specs/blocks/<block>.toml`. Each row
//! states a write the guest makes and the read it then polls, the bound within which the model
//! has to answer, the guest functions observed polling it and the corpus images that do.
//!
//! Two consumers: a block's own package turns the rows of its block into model tests
//! (`of_block`), and the hang detector takes a row polled by a corpus image as a firmware-level
//! expectation. The rows are the specification, not the model: a row says what the
//! guest must observe, never how the peripheral produces it.
//!
//! `specs/README.md` section 3.2 documents the columns and the citation of every row; the
//! `provenance` of a row stays in the spec file, which is where a reader of a failing test goes.

"#;

const TYPES: &str = r#"/// Bound of a busy-wait row: how long the guest may poll before the model has to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Within {
    /// The same bound under every timing profile (`same_access`, `never`, a duration).
    All(&'static str),
    /// One bound per timing profile: a real device is slower than the `fast` profile.
    PerProfile {
        /// Bound under the `fast` profile.
        fast: &'static str,
        /// Bound under the `device` profile.
        device: &'static str,
    },
}

/// A row's kind: a wait the model has to satisfy, or a poll that must never be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitKind {
    /// The guest polls until the expectation holds, so the model has to make it hold.
    Wait,
    /// The guest must never reach this poll: reaching it is a modeling bug.
    Tripwire,
}

/// One busy-wait row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitSpec {
    /// `<block>.<name>`, unique across every block file.
    pub id: &'static str,
    /// Wait or tripwire.
    pub kind: WaitKind,
    /// True for a row of the seed set.
    pub seed: bool,
    /// `block` of the spec file, or its `chip` for a board chip.
    pub block: &'static str,
    /// Register the guest polls, as an IDF macro name without the `_REG` suffix.
    pub register: &'static str,
    /// Field or comma-separated fields of that register.
    pub field: &'static str,
    /// What the guest does first, in prose.
    pub trigger: &'static str,
    /// What the guest then polls for, in prose.
    pub expect: &'static str,
    /// How long the guest may poll.
    pub within: Within,
    /// Guest functions observed polling the row.
    pub polled_at: &'static [&'static str],
    /// Corpus images that reach the row, by corpus id.
    pub images: &'static [&'static str],
    /// Milestone that owns the row (`M<n>` or `LATER`).
    pub milestone: &'static str,
}

"#;

const LOOKUPS: &str = r#"
/// Row with this id, or `None`.
pub fn find(id: &str) -> Option<&'static WaitSpec> {
    WAITS.iter().find(|w| w.id == id)
}

/// Rows of one block or board chip, in table order.
pub fn of_block(block: &str) -> impl Iterator<Item = &'static WaitSpec> {
    WAITS.iter().filter(move |w| w.block == block)
}

/// Rows of the seed set, in table order.
pub fn seeds() -> impl Iterator<Item = &'static WaitSpec> {
    WAITS.iter().filter(|w| w.seed)
}
"#;

const TESTS: &str = r#"
#[cfg(test)]
mod tests {
    use super::*;

    /// The seed set: 29 rows, exactly one of them a tripwire.
    #[test]
    fn the_seed_set_has_29_rows_and_one_tripwire() {
        assert_eq!(seeds().count(), SEED_COUNT);
        assert_eq!(SEED_COUNT, 29);
        assert_eq!(
            seeds().filter(|w| w.kind == WaitKind::Tripwire).count(),
            1,
            "the seed set has one tripwire row"
        );
    }

    /// Every id is unique and names its own block, so `of_block` and `find` cannot collide.
    #[test]
    fn ids_are_unique_and_name_their_block() {
        for (i, w) in WAITS.iter().enumerate() {
            assert_eq!(
                w.id.split('.').next(),
                Some(w.block),
                "{} does not start with its block",
                w.id
            );
            assert!(w.id.len() > w.block.len() + 1, "{} has no name", w.id);
            assert!(
                WAITS[..i].iter().all(|p| p.id != w.id),
                "two rows with id {}",
                w.id
            );
            assert_eq!(find(w.id), Some(w));
        }
    }

    /// Every row carries the columns a model test and the hang detector read.
    #[test]
    fn every_row_is_complete() {
        for w in &WAITS {
            let at = w.id;
            assert!(!w.register.is_empty(), "{at}: no register");
            assert!(!w.field.is_empty(), "{at}: no field");
            assert!(!w.trigger.is_empty(), "{at}: no trigger");
            assert!(!w.expect.is_empty(), "{at}: no expectation");
            assert!(
                w.milestone == "LATER" || w.milestone.starts_with('M'),
                "{at}: milestone {}",
                w.milestone
            );
            match w.within {
                Within::All(bound) => assert!(!bound.is_empty(), "{at}: empty bound"),
                Within::PerProfile { fast, device } => {
                    assert!(!fast.is_empty() && !device.is_empty(), "{at}: empty bound");
                }
            }
            for site in w.polled_at {
                assert!(!site.is_empty(), "{at}: empty polled_at entry");
            }
            for image in w.images {
                assert!(!image.is_empty(), "{at}: empty images entry");
            }
        }
    }

    /// `of_block` covers the table exactly once.
    #[test]
    fn of_block_partitions_the_table() {
        let mut seen = 0;
        for (i, w) in WAITS.iter().enumerate() {
            if WAITS[..i].iter().any(|p| p.block == w.block) {
                continue;
            }
            seen += of_block(w.block).count();
        }
        assert_eq!(seen, WAIT_COUNT);
    }
}
"#;
