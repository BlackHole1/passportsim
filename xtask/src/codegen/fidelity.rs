//! `docs/fidelity.md`: the generated fidelity ledger: block, register or hook, class, citation,
//! test ids.
//!
//! It is an aggregate build output and not committed (`.gitignore`), so
//! `xtask codegen --check` compares it only when a previous run left one behind; a fresh checkout
//! has none.
//!
//! # What the rows are
//!
//! A class is spec data (`super::blocks`), so the ledger is rendered from `specs/blocks/*.toml`
//! and the merged register tables rather than from a run. There are three kinds of row:
//!
//! - **Claims.** One row per `[[overrides]]` row that carries a class, with the registers it
//!   classes and its `provenance`, which is where the spec citation and the test ids of the
//!   promotion rules live. A `registers` glob and an `offsets` span stay one row, so a
//!   rule for 128 MMU entries reads as one rule.
//! - **Inherited.** A register with a `reset_domains`, `wait` or `stable_read` row and no
//!   `overrides` row of its own takes the block's class, cited by the block header.
//! - **Open items.** Every claim whose provenance says `UNVERIFIED` (`specs/README.md` rule 5),
//!   the list to review per release.
//!
//! Nothing here reads a run, a data root or a device value: the ledger of a run is
//! `fidelity.json` beside its artifacts, and a first touch never carries a value
//! (`pemu_core::fidelity`).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::blocks::Spec;
use super::regs::Block;

/// Path of the generated file, relative to the workspace root.
pub const OUTPUT_PATH: &str = "docs/fidelity.md";

/// The fidelity classes, as `(class, meaning, promotion rule)`. It mirrors
/// `pemu_core::fidelity::Fidelity::{meaning, promotion_rule}`, which the receipts use.
const CLASSES: [(&str, &str, &str); 4] = [
    (
        "A",
        "matches the device",
        "a test tied to a device capture id",
    ),
    (
        "B",
        "matches the spec and oracles",
        "a spec citation plus a passing oracle comparison or an IDF-LL-derived test",
    ),
    ("C", "deliberate approximation", "a declared rationale"),
    ("U", "unmodeled or unclaimed", "the default"),
];

/// Renders the whole document from the block files and the merged register tables.
pub fn render(specs: &[Spec], blocks: &[Block]) -> String {
    let mut out = String::new();
    out.push_str(HEAD);
    per_class(&mut out, blocks);
    per_block(&mut out, specs, blocks);
    claims(&mut out, specs, blocks);
    inherited(&mut out, specs, blocks);
    open_items(&mut out, specs);
    out
}

/// How many registers of the merged tables carry each class.
fn counts(regs: impl Iterator<Item = &'static str>) -> BTreeMap<&'static str, usize> {
    let mut out: BTreeMap<&'static str, usize> = BTreeMap::new();
    for class in regs {
        *out.entry(class).or_default() += 1;
    }
    out
}

fn per_class(out: &mut String, blocks: &[Block]) {
    let n = counts(blocks.iter().flat_map(|b| b.regs.iter().map(|r| r.class)));
    out.push_str(
        "## Classes\n\n| Class | Meaning | Promotion rule | Registers |\n|---|---|---|---|\n",
    );
    for (class, meaning, rule) in CLASSES {
        let _ = writeln!(
            out,
            "| {class} | {meaning} | {rule} | {} |",
            n.get(class).copied().unwrap_or(0)
        );
    }
    let _ = writeln!(
        out,
        "\nCounted over the {} registers of the generated tables. A block with no rows in \
         `specs/c3-registers.csv` has no table and is not counted here; its classes are the \
         `offsets` rows of the next section.\n",
        blocks.iter().map(|b| b.regs.len()).sum::<usize>()
    );
}

fn per_block(out: &mut String, specs: &[Spec], blocks: &[Block]) {
    out.push_str(
        "## Blocks\n\nThe block's own class is the one a register inherits when a row of the file \
         names it and no `overrides` row classes it.\n\n\
         | Block | Class | Milestone | Registers | A | B | C | U |\n|---|---|---|---|---|---|---|---|\n",
    );
    for spec in specs.iter().filter(|s| !s.is_chip) {
        let table = blocks.iter().find(|b| b.name == spec.name);
        let n = counts(
            table
                .into_iter()
                .flat_map(|b| b.regs.iter().map(|r| r.class)),
        );
        let total = match table {
            Some(b) => b.regs.len().to_string(),
            None => "no table".to_string(),
        };
        let cell = |c: &str| match table {
            Some(_) => n.get(c).copied().unwrap_or(0).to_string(),
            None => "-".to_string(),
        };
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {total} | {} | {} | {} | {} |",
            spec.name,
            spec.class,
            spec.milestone,
            cell("A"),
            cell("B"),
            cell("C"),
            cell("U"),
        );
    }
    out.push('\n');
}

/// The subject of a claim row and how many registers or offsets it covers.
fn subject(spec: &Spec, blocks: &[Block], row: &super::blocks::OverrideRow) -> (String, String) {
    if !row.offsets.is_empty() {
        let (first, last) = (row.offsets[0], row.offsets[row.offsets.len() - 1]);
        let span = if row.offsets.len() == 1 {
            format!("`{}` (+{first:#05X})", row.register)
        } else {
            format!("`{}` (+{first:#05X} to +{last:#05X})", row.register)
        };
        return (span, format!("{} offsets", row.offsets.len()));
    }
    if row.glob {
        let n = blocks.iter().find(|b| b.name == spec.name).map_or(0, |b| {
            b.regs.iter().filter(|r| spec.classes(row, r.name)).count()
        });
        return (format!("`{}`", row.register), format!("{n} registers"));
    }
    if row.register == "*" {
        return ("the whole block, in prose".to_string(), "none".to_string());
    }
    (format!("`{}`", row.register), "1 register".to_string())
}

fn claims(out: &mut String, specs: &[Spec], blocks: &[Block]) {
    out.push_str(
        "## Claims\n\nOne row per `[[overrides]]` row of `specs/blocks/`, in file order. \
         `Citation` is the row's `provenance`, which carries the spec citation and the test ids \
         the promotion rule of that class asks for. A row that classes nothing, a \
         `register = \"*\"` block-wide rule, is listed with `none`.\n\n\
         | Block | Subject | Class | Classes | Citation |\n|---|---|---|---|---|\n",
    );
    for spec in specs {
        for row in &spec.overrides {
            let (what, n) = subject(spec, blocks, row);
            let _ = writeln!(
                out,
                "| `{}` | {what} | {} | {n} | {} |",
                spec.name,
                row.class,
                cell(&row.provenance)
            );
        }
    }
    out.push('\n');
}

fn inherited(out: &mut String, specs: &[Spec], blocks: &[Block]) {
    out.push_str(
        "## Registers that inherit the block class\n\nA register with a `reset_domains`, `wait` \
         or `stable_read` row of its own and no `overrides` row takes the class of its block, \
         cited by the block header.\n\n\
         | Block | Register | Offset | Class | Citation |\n|---|---|---|---|---|\n",
    );
    for spec in specs.iter().filter(|s| !s.is_chip) {
        let Some(table) = blocks.iter().find(|b| b.name == spec.name) else {
            continue;
        };
        for r in &table.regs {
            if r.class == "U" || spec.overrides.iter().any(|o| o.register == r.name) {
                continue;
            }
            if !spec.inherits(r.name) {
                continue;
            }
            let _ = writeln!(
                out,
                "| `{}` | `{}` | +{:#05X} | {} | {} |",
                spec.name,
                r.name,
                r.off,
                r.class,
                cell(&spec.provenance)
            );
        }
    }
    out.push('\n');
}

fn open_items(out: &mut String, specs: &[Spec]) {
    out.push_str(
        "## Open items\n\nEvery header and every `[[overrides]]` row whose `provenance` says \
         `UNVERIFIED` (`specs/README.md` citation rule 5).\n\n\
         | Block | Subject | Class | Citation |\n|---|---|---|---|\n",
    );
    for spec in specs {
        if spec.provenance.contains("UNVERIFIED") {
            let _ = writeln!(
                out,
                "| `{}` | the block header | {} | {} |",
                spec.name,
                spec.class,
                cell(&spec.provenance)
            );
        }
        for row in spec
            .overrides
            .iter()
            .filter(|o| o.provenance.contains("UNVERIFIED"))
        {
            let _ = writeln!(
                out,
                "| `{}` | `{}` | {} | {} |",
                spec.name,
                row.register,
                row.class,
                cell(&row.provenance)
            );
        }
    }
}

/// A `provenance` string as one Markdown table cell: pipes escaped, newlines folded.
fn cell(text: &str) -> String {
    text.replace('|', "\\|").replace('\n', " ")
}

/// Everything above the first generated section.
const HEAD: &str = concat!(
    "<!-- generated by cargo xtask codegen; do not edit. -->\n\n",
    "# Fidelity ledger\n\n",
    "Which register of which block the emulator claims to model, how \
     strongly, and on what evidence. Generated by `cargo xtask codegen` from `",
    "specs/blocks/*.toml",
    "` and `specs/c3-registers.csv`; it is a build output and is not committed.\n\n",
    "A class is a claim about what the model does, not about what the silicon does. `U` is the \
     absence of a claim and the default: a strict milestone fails on the first touch of a `U` \
     register, and the coverage gate needs a class other than `U` or an allowlist entry for \
     every register a corpus image touches.\n\n",
);

#[cfg(test)]
mod tests {
    use super::super::{blocks, regs};
    use super::*;

    /// The ledger names the block, the subject, the class and the citation, and every class of a
    /// register reaches it from the spec files.
    #[test]
    fn the_ledger_carries_every_claim_of_the_block_files() {
        let root = crate::codegen::workspace_root();
        let rows = super::super::csv::parse(
            &std::fs::read_to_string(root.join(super::super::csv::SPEC_PATH)).unwrap(),
        )
        .unwrap();
        let mut blocks = regs::group(&rows).unwrap();
        let specs = blocks::load(&root).unwrap();
        blocks::apply(&mut blocks, &specs).unwrap();
        let text = render(&specs, &blocks);
        for want in [
            "# Fidelity ledger",
            "## Classes",
            "## Blocks",
            "## Claims",
            "## Registers that inherit the block class",
            "## Open items",
            "| `mmu` | `MMU entry table` (+0x000 to +0x1FC) | B | 128 offsets |",
            "| `gpio` | `GPIO_STRAP` | A | 1 register |",
        ] {
            assert!(text.contains(want), "{want}");
        }
        let claims: usize = specs.iter().map(|s| s.overrides.len()).sum();
        let rows = text.lines().filter(|l| l.starts_with("| `")).count();
        assert!(rows > claims, "every overrides row is a claim row");
        assert!(
            text.contains(super::super::blocks::SPEC_DIR),
            "the header names the spec directory"
        );
        for line in text.lines().filter(|l| l.starts_with("| `")) {
            assert!(
                !line.contains("\n"),
                "a citation must not break its row: {line}"
            );
        }
    }

    /// A `provenance` with a pipe cannot break the table it is printed in.
    #[test]
    fn a_citation_is_escaped_into_one_cell() {
        assert_eq!(cell("a | b\nc"), "a \\| b c");
    }
}
