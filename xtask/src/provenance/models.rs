//! Rule `model-no-citation`: every model file header cites a public source or says `UNVERIFIED`
//! (`CONTRIBUTING.md#clean-room`).
//!
//! A model file implements emulated silicon or guest behavior, so a reviewer must be able to walk
//! from any of its lines back to a source. The model files are every `.rs` file under [`SCOPES`]:
//! the CPU (`pemu-rv32`), the whole SoC crate (address space, MMU, interrupt fabric, `wiring/`,
//! `periph/` including the `c3_devices!` table, and the generated `gen/` tables, whose citations
//! come from the spec rows), board chips (`pemu-board`) and HLE (`pemu-hle`). A block still on the
//! store-only stub is excluded: with no model of its own it has no rows to cite yet. A model that
//! moves into another crate adds its directory to [`SCOPES`].
//!
//! The header is the leading run of `//!` lines; one of them must pass [`has_citation`].

use super::{Finding, Rule, SourceFile};

/// Directories whose `.rs` files are model files, relative to the repository root. Each is read
/// recursively, so `crates/pemu-soc-c3/src` covers `periph/`, `wiring/` and `gen/` as well.
pub const SCOPES: &[&str] = &[
    "crates/pemu-rv32/src",
    "crates/pemu-soc-c3/src",
    "crates/pemu-board/src",
    "crates/pemu-hle/src",
];

/// True when a file of [`SCOPES`] is a model file rather than a store-only stub.
pub fn is_model(path: &str, text: &str) -> bool {
    let stub = path.ends_with("/store_only.rs") || store_only_alias(text);
    !stub
}

/// True when the file's own model is the `StoreOnly<B>` generic, which a block file
/// declares as `pub type <Name> = StoreOnly<super::block::<Name>>;`. The test is the alias and not
/// a plain mention of `StoreOnly<`, so a file that names the generic in a comment or lists
/// store-only blocks in the `c3_devices!` table (`periph/mod.rs`) stays a model file.
fn store_only_alias(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("pub type") && line.contains("= StoreOnly<")
    })
}

/// Checks the model files, in the order given.
pub fn scan(files: &[SourceFile]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in files {
        let header = header(&file.text);
        if !header.iter().any(|(_, line)| has_citation(line)) {
            findings.push(Finding {
                rule: Rule::ModelNoCitation,
                file: file.path.clone(),
                line: 1,
                detail: "model file header cites no public source: a `specs/` row, the TRM, a \
                         datasheet, an IDF file or a probe, or says `UNVERIFIED` \
                         (`CONTRIBUTING.md#clean-room`)"
                    .to_string(),
            });
        }
    }
    findings
}

/// The file header: the leading run of `//!` lines, after any blank or attribute lines.
fn header(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .skip_while(|(_, line)| {
            let trimmed = line.trim();
            trimmed.is_empty()
                || trimmed.starts_with("#![")
                || (trimmed.starts_with("//") && !trimmed.starts_with("//!"))
        })
        .take_while(|(_, line)| line.trim_start().starts_with("//!"))
        .map(|(index, line)| (index + 1, line))
        .collect()
}

/// True when `line` names a public source or says `UNVERIFIED`:
///
/// - a `specs/` path, which is how a file cites the spec rows it implements;
/// - an ESP-IDF file (`IDF hal/esp32c3/include/hal/spi_ll.h:267`, or any `.h` or `.c` path), the
///   TRM or a datasheet;
/// - a probe under `probes/`.
pub fn has_citation(line: &str) -> bool {
    let words = [
        "specs/",
        "TRM",
        "datasheet",
        "IDF ",
        "probes/",
        "UNVERIFIED",
    ];
    if words.iter().any(|word| line.contains(word)) {
        return true;
    }
    line.contains(".h") && line.contains('/') || line.contains(".c:")
}
