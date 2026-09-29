//! `specs/irq-sources.toml` and the generated `crates/pemu-core/src/irq_source.rs`.

use std::fmt::Write as _;

use super::GENERATED_HEADER;

/// Path of the spec table, relative to the workspace root.
pub const SPEC_PATH: &str = "specs/irq-sources.toml";
/// Path of the generated file, relative to the workspace root.
pub const OUTPUT_PATH: &str = "crates/pemu-core/src/irq_source.rs";
/// IDF `ETS_MAX_INTR_SOURCE` (`soc/interrupts.h`).
pub const SOURCE_COUNT: usize = 62;
/// Rows pinned by the test `irq_numbering_matches_map_offsets`.
pub const PINNED: [(&str, u8, u32); 4] = [
    ("SYSTIMER_TARGET2", 39, 0x09C),
    ("SPI_MEM_REJECT", 40, 0x0A0),
    ("FROM_CPU_INTR0", 50, 0x0C8),
    ("CACHE_CORE0_ACS", 61, 0x0F4),
];

/// One `[[source]]` row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    pub name: String,
    pub number: u8,
    pub map_off: u32,
    pub idf: String,
    pub map_reg: String,
    pub cite: String,
}

/// Parses and validates the spec table.
pub fn parse(text: &str) -> Result<Vec<Source>, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("{SPEC_PATH}: {e}"))?;
    let rows = table
        .get("source")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| format!("{SPEC_PATH}: no [[source]] rows"))?;
    let mut sources = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let err = |what: &str| format!("{SPEC_PATH}: source row {i}: {what}");
        let t = row.as_table().ok_or_else(|| err("not a table"))?;
        let text = |key: &str| {
            t.get(key)
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| err(&format!("missing string `{key}`")))
        };
        let int = |key: &str| {
            t.get(key)
                .and_then(toml::Value::as_integer)
                .ok_or_else(|| err(&format!("missing integer `{key}`")))
        };
        if let Some(extra) = t.keys().find(|k| {
            !["name", "number", "map_off", "idf", "map_reg", "cite"].contains(&k.as_str())
        }) {
            return Err(err(&format!("unknown key `{extra}`")));
        }
        sources.push(Source {
            name: text("name")?,
            number: u8::try_from(int("number")?).map_err(|_| err("number out of range"))?,
            map_off: u32::try_from(int("map_off")?).map_err(|_| err("map_off out of range"))?,
            idf: text("idf")?,
            map_reg: text("map_reg")?,
            cite: text("cite")?,
        });
    }
    validate(&sources)?;
    Ok(sources)
}

/// Checks the rows: exactly `SOURCE_COUNT` rows numbered `0..SOURCE_COUNT` in order, unique
/// constant names, `map_off / 4 == number`, and the `PINNED` rows.
pub fn validate(sources: &[Source]) -> Result<(), String> {
    if sources.len() != SOURCE_COUNT {
        return Err(format!(
            "expected {SOURCE_COUNT} sources, found {}",
            sources.len()
        ));
    }
    for (i, s) in sources.iter().enumerate() {
        let ident = !s.name.is_empty()
            && !s.name.starts_with(|c: char| c.is_ascii_digit())
            && s.name
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
        if !ident {
            return Err(format!("source `{}`: not a constant name", s.name));
        }
        if usize::from(s.number) != i {
            return Err(format!(
                "source `{}`: number {} at row {i}",
                s.name, s.number
            ));
        }
        if !s.map_off.is_multiple_of(4) || s.map_off / 4 != u32::from(s.number) {
            return Err(format!(
                "source `{}`: MAP offset {:#05x} does not match number {}",
                s.name, s.map_off, s.number
            ));
        }
        if sources[..i].iter().any(|p| p.name == s.name) {
            return Err(format!("source `{}`: duplicate name", s.name));
        }
        for text in [&s.idf, &s.map_reg, &s.cite] {
            if text.contains(['"', '\\', '\n']) {
                return Err(format!(
                    "source `{}`: unsupported character in `{text}`",
                    s.name
                ));
            }
        }
    }
    for (name, number, off) in PINNED {
        let row = &sources[usize::from(number)];
        if row.name != name || row.map_off != off {
            return Err(format!(
                "pinned source {name} {number} at {off:#05x} does not match"
            ));
        }
    }
    Ok(())
}

/// Renders the spec table.
pub fn render_toml(sources: &[Source]) -> String {
    let mut out = String::from(
        "# Interrupt sources of the ESP32-C3 interrupt matrix.\n\
         # Written by `cargo xtask codegen import` from ESP-IDF v5.5.3 `soc/interrupts.h`\n\
         # (enumerator values) and `interrupt_core0_reg.h` (MAP register offsets), paired by name.\n\
         # `cargo xtask codegen` checks every row and generates crates/pemu-core/src/irq_source.rs.\n",
    );
    for s in sources {
        let _ = write!(
            out,
            "\n[[source]]\nname = \"{}\"\nnumber = {}\nmap_off = 0x{:03X}\nidf = \"{}\"\n\
             map_reg = \"{}\"\ncite = \"{}\"\n",
            s.name, s.number, s.map_off, s.idf, s.map_reg, s.cite
        );
    }
    out
}

/// Renders `irq_source.rs` (before rustfmt).
pub fn render(sources: &[Source]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{GENERATED_HEADER}");
    out.push_str(
        "//!\n\
         //! Interrupt sources of the ESP32-C3 interrupt matrix, generated from\n\
         //! `specs/irq-sources.toml`. Numbers are the ESP-IDF v5.5.3 `periph_interrupt_t` values of\n\
         //! `soc/esp32c3/include/soc/interrupts.h`, paired by name with the\n\
         //! `INTERRUPT_CORE0_*_MAP_REG` registers of `soc/esp32c3/register/soc/interrupt_core0_reg.h`\n\
         //! and checked against their offsets.\n\n\
         use serde::{Deserialize, Serialize};\n\n\
         /// One of the 62 interrupt sources: the IDF `periph_interrupt_t` number, generated by name,\n\
         /// never by position.\n\
         #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]\n\
         pub struct IrqSource(pub u8);\n\n\
         /// Number of interrupt sources, IDF `ETS_MAX_INTR_SOURCE`.\n",
    );
    let _ = writeln!(out, "pub const SOURCE_COUNT: usize = {};\n", sources.len());
    out.push_str(
        "/// One row of `specs/irq-sources.toml`.\n\
         #[derive(Copy, Clone, PartialEq, Eq, Debug)]\n\
         pub struct IrqSourceInfo {\n\
         /// Constant name in [`irq`].\n pub name: &'static str,\n\
         /// Source number.\n pub source: IrqSource,\n\
         /// Offset of the source's MAP register from the INTERRUPT_CORE0 base (0x600C2000).\n\
         pub map_off: u32,\n}\n\n\
         /// Every source in number order: `SOURCES[n].source == IrqSource(n)`.\n\
         pub const SOURCES: [IrqSourceInfo; SOURCE_COUNT] = [\n",
    );
    for s in sources {
        let _ = writeln!(
            out,
            "IrqSourceInfo {{ name: \"{0}\", source: irq::{0}, map_off: 0x{1:03X} }},",
            s.name, s.map_off
        );
    }
    out.push_str("];\n\n/// Named interrupt sources.\npub mod irq {\nuse super::IrqSource;\n");
    for s in sources {
        let _ = write!(
            out,
            "\n/// `{}`, MAP register `{}` (0x{:03X}).\npub const {}: IrqSource = IrqSource({});\n",
            s.idf, s.map_reg, s.map_off, s.name, s.number
        );
    }
    out.push_str(
        "}\n\n#[cfg(test)]\nmod tests {\nuse super::*;\n\n\
         /// Numbering by name agrees with the MAP register offsets.\n\
         #[test]\nfn irq_numbering_matches_map_offsets() {\n",
    );
    for (name, number, off) in PINNED {
        let _ = writeln!(
            out,
            "assert_eq!(irq::{name}, IrqSource({number}));\n\
             assert_eq!(SOURCES[{number}].map_off, 0x{off:03X});"
        );
    }
    out.push_str(
        "for (n, s) in SOURCES.iter().enumerate() {\n\
         assert_eq!(usize::from(s.source.0), n, \"{}\", s.name);\n\
         assert!(s.map_off.is_multiple_of(4), \"{}\", s.name);\n\
         assert_eq!(s.map_off / 4, u32::from(s.source.0), \"{}\", s.name);\n\
         }\n}\n}\n",
    );
    out
}
