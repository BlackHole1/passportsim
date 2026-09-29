//! `crates/pemu-soc-c3/src/gen/classes.rs`: the fidelity class of a block offset, for the blocks a
//! generated `RegSpec` table cannot class.
//!
//! Most blocks carry their classes in the `class` column of `gen/regs_<block>.rs`, which
//! `super::blocks` merges from the `[[overrides]]` rows of `specs/blocks/<block>.toml`. A block
//! with no rows in `specs/c3-registers.csv` has no such table: `mmu` is a flat array of 128
//! entries, and `sha` and `regi2c` have no field layout in the IDF headers. An `[[overrides]]` row
//! of such a block may carry `offsets`; this module turns those rows into one `pub mod <block>`
//! with a `class_at`, which the block's model calls from `Peripheral::fidelity`, so a class is
//! spec data for every block. An offset no row names is `Fidelity::U`.

use std::fmt::Write as _;

use super::blocks::Spec;

/// Path of the generated file, relative to the workspace root.
pub const OUTPUT_PATH: &str = "crates/pemu-soc-c3/src/gen/classes.rs";

/// Module name of the generated file inside `crates/pemu-soc-c3/src/gen/mod.rs`.
pub const MODULE: &str = "classes";

/// The blocks that have `offsets` rows, in file-name order.
pub fn classed(specs: &[Spec]) -> Vec<&Spec> {
    specs
        .iter()
        .filter(|s| !s.is_chip && s.overrides.iter().any(|o| !o.offsets.is_empty()))
        .collect()
}

/// Renders the whole file from the `offsets` rows of every block file.
pub fn render(specs: &[Spec]) -> Result<String, String> {
    let mut out = String::new();
    let _ = writeln!(out, "{}", super::GENERATED_HEADER);
    out.push_str(FILE_DOC);
    let blocks = classed(specs);
    if !blocks.is_empty() {
        out.push_str("use pemu_core::fidelity::Fidelity;\n");
    }
    for spec in blocks {
        let rows = spec.offset_classes()?;
        let _ = write!(
            out,
            "\n/// Fidelity classes of the `{name}` block, from the `offsets` rows of\n\
             /// `{SPEC_DIR}/{file}`.\n\
             pub mod {name} {{\n\
             {INDENT}use super::Fidelity;\n\n\
             {INDENT}/// The class of every offset a row of the block file names, ascending by\n\
             {INDENT}/// offset. An offset that is not here is `Fidelity::U`.\n\
             {INDENT}pub const CLASSES: [(u32, Fidelity); {count}] = [\n",
            name = spec.name,
            file = spec.file,
            count = rows.len(),
            SPEC_DIR = super::blocks::SPEC_DIR,
            INDENT = "    ",
        );
        for (off, class) in &rows {
            let _ = writeln!(out, "        ({off:#06X}, Fidelity::{class}),");
        }
        out.push_str(
            "    ];\n\n\
             \x20   /// The class of the register word holding block offset `off`, `Fidelity::U`\n\
             \x20   /// where no row of the block file names it.\n\
             \x20   pub fn class_at(off: u32) -> Fidelity {\n\
             \x20       match CLASSES.binary_search_by_key(&(off & !3), |(o, _)| *o) {\n\
             \x20           Ok(i) => CLASSES[i].1,\n\
             \x20           Err(_) => Fidelity::U,\n\
             \x20       }\n\
             \x20   }\n\
             }\n",
        );
    }
    Ok(out)
}

/// Module documentation of the generated file.
const FILE_DOC: &str = "//!\n\
    //! Fidelity classes by block offset, for the blocks that have no rows in\n\
    //! `specs/c3-registers.csv` and so no generated `RegSpec` table to carry a `class` column.\n\
    //! One module per such block, from the `[[overrides]]` rows of\n\
    //! `specs/blocks/<block>.toml` that carry `offsets`; the block's model answers\n\
    //! `Peripheral::fidelity` out of it, so a class is spec data for every block and never a\n\
    //! constant in a model. An offset no row names is `Fidelity::U`.\n\
    //!\n\
    //! `xtask/src/codegen/classes.rs` renders this file.\n\n";
