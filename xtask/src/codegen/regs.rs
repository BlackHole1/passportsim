//! Per-block register tables generated from `specs/c3-registers.csv` into
//! `crates/pemu-soc-c3/src/gen/regs_<block>.rs`, plus the `gen/mod.rs` that declares them.
//!
//! One module per `c3_devices!` block that has rows in the CSV; `codegen.rs` lists the blocks the
//! importer cannot read. `RegSpec` fields the CSV does not carry come from
//! `specs/blocks/<block>.toml` through `blocks::apply`: `domain` from the block's `reset_domains`
//! rows as a bit mask of `ResetScope` (UNVERIFIED encoding), `stable_read`, `class`, and a field
//! reset from an `overrides` row with an integer value.

use std::fmt::Write as _;

use super::csv::{Row, ScopeReset};
use super::import::BLOCKS;

/// Directory of the generated tables, relative to the workspace root.
pub const OUTPUT_DIR: &str = "crates/pemu-soc-c3/src/gen";

/// Bit of `ResetScope::Chip` in a `RegSpec::domain` mask (UNVERIFIED encoding, see `gen/mod.rs`).
pub const SCOPE_CHIP: u8 = 0x1;
/// Bit of `ResetScope::System`.
pub const SCOPE_SYSTEM: u8 = 0x2;
/// Bit of `ResetScope::Core`.
pub const SCOPE_CORE: u8 = 0x4;

/// One register of one block: the CSV rows of its fields, in field order.
pub struct Register<'a> {
    /// IDF register macro name without the `_REG` suffix.
    pub name: &'a str,
    /// Offset from the block base.
    pub off: u32,
    /// Value after a chip reset: the OR of the shifted field reset values.
    pub reset: u32,
    /// Mask of the `ResetScope` values that restore `reset`.
    pub domain: u8,
    /// Eligible for an oracle read comparison, from the block's `stable_read` rows.
    pub stable_read: bool,
    /// `Fidelity` variant name, from the block file (`blocks::apply`).
    pub class: &'static str,
    /// Citation of the register's first field row, then the block file rows that changed the
    /// columns the CSV does not carry (`blocks::apply`).
    pub cite: String,
    /// Field rows, sorted by shift.
    pub fields: Vec<&'a Row>,
    /// Reset values of `overrides` rows, by field name: they win over the CSV reset.
    pub field_resets: Vec<(String, u32)>,
}

impl Register<'_> {
    /// Reset value of one field: its `overrides` row if the block file has one, else the CSV
    /// column.
    pub fn field_reset(&self, f: &Row) -> u32 {
        self.field_resets
            .iter()
            .find(|(name, _)| *name == f.field)
            .map_or(f.reset_chip, |(_, v)| *v)
    }

    /// Recomputes `reset` as the OR of the shifted field resets, after `overrides` rows applied.
    pub fn recompute_reset(&mut self) {
        self.reset = self
            .fields
            .iter()
            .fold(0, |acc, f| acc | (self.field_reset(f) << f.shift));
    }
}

/// One `c3_devices!` block that has rows in the CSV.
pub struct Block<'a> {
    /// `c3_devices!` row name.
    pub name: &'a str,
    /// Window size of the row.
    pub size: u32,
    /// IDF register headers the rows come from, each with the offset of its base inside the block
    /// window (`import::BLOCKS`).
    pub headers: &'static [(&'static str, u32)],
    /// Registers in offset order.
    pub regs: Vec<Register<'a>>,
}

impl Block<'_> {
    /// Name of the generated module.
    pub fn module(&self) -> String {
        format!("regs_{}", self.name)
    }

    /// Number of field rows of the block.
    pub fn field_count(&self) -> usize {
        self.regs.iter().map(|r| r.fields.len()).sum()
    }
}

/// Name of the `gen/mod.rs` constant for a domain mask, spelled from the scopes it holds.
pub fn domain_const(mask: u8) -> String {
    if mask == 0 {
        return String::from("DOMAIN_NONE");
    }
    let mut name = String::from("DOMAIN");
    for (bit, scope) in [
        (SCOPE_CHIP, "CHIP"),
        (SCOPE_SYSTEM, "SYSTEM"),
        (SCOPE_CORE, "CORE"),
    ] {
        if mask & bit != 0 {
            name.push('_');
            name.push_str(scope);
        }
    }
    name
}

/// Groups the CSV rows into blocks and registers, in `c3_devices!` table order, and checks what
/// the generated tables assert: offsets word-aligned, unique, ascending and inside the block
/// window; fields sorted, non-overlapping and inside 32 bits; per-scope reset values that agree
/// with the chip reset, so one `RegSpec::reset` serves every scope of the domain.
pub fn group(rows: &[Row]) -> Result<Vec<Block<'_>>, String> {
    let mut blocks = Vec::new();
    for (name, size, headers) in BLOCKS {
        let mut regs: Vec<Register> = Vec::new();
        for row in rows.iter().filter(|r| r.block == name) {
            match regs.last_mut() {
                Some(last) if last.name == row.register => {
                    if row.offset != last.off {
                        return Err(format!("{name}.{}: two offsets", row.register));
                    }
                    last.fields.push(row);
                }
                _ => {
                    if regs.iter().any(|r| r.name == row.register) {
                        return Err(format!("{name}.{}: rows not grouped", row.register));
                    }
                    regs.push(Register {
                        name: &row.register,
                        off: row.offset,
                        reset: 0,
                        domain: 0,
                        stable_read: false,
                        class: "U",
                        cite: row.cite.clone(),
                        fields: vec![row],
                        field_resets: Vec::new(),
                    });
                }
            }
        }
        if regs.is_empty() {
            continue;
        }
        for reg in &mut regs {
            finish(name, size, reg)?;
        }
        for pair in regs.windows(2) {
            if pair[0].off >= pair[1].off {
                return Err(format!(
                    "{name}: {} at {:#05X} is not before {} at {:#05X}",
                    pair[0].name, pair[0].off, pair[1].name, pair[1].off
                ));
            }
        }
        blocks.push(Block {
            name,
            size,
            headers,
            regs,
        });
    }
    Ok(blocks)
}

/// Computes `reset` and `domain` of one register and checks its offset and its fields.
fn finish(block: &str, size: u32, reg: &mut Register) -> Result<(), String> {
    let at = |what: &str| format!("{block}.{}: {what}", reg.name);
    if !reg.off.is_multiple_of(4) || reg.off + 4 > size {
        return Err(at("offset not word-aligned or outside the block window"));
    }
    let mut mask = 0u32;
    let mut reset = 0u32;
    let mut prev_shift = None;
    for f in &reg.fields {
        if Some(f.shift) <= prev_shift {
            return Err(at(&format!("field {} out of shift order", f.field)));
        }
        prev_shift = Some(f.shift);
        let bits = field_bits(f);
        if mask & bits != 0 {
            return Err(at(&format!("field {} overlaps a previous field", f.field)));
        }
        mask |= bits;
        reset |= f.reset_chip << f.shift;
    }
    reg.reset = reset;
    reg.domain = domain_mask(&reg.fields).ok_or_else(|| {
        at(
            "fields disagree on which reset scopes restore them, or a scope value differs \
            from the chip reset",
        )
    })?;
    Ok(())
}

/// Renders `regs_<block>.rs` (before rustfmt).
pub fn render_block(b: &Block) -> String {
    let mut out = String::new();
    let mut domains: Vec<String> = b.regs.iter().map(|r| domain_const(r.domain)).collect();
    domains.sort_unstable();
    domains.dedup();
    let _ = writeln!(out, "{}", super::GENERATED_HEADER);
    let _ = write!(
        out,
        "//!\n\
         //! Register table of the `{name}` block: {regs} registers, {fields} fields.\n\
         {sources}\
         //! `domain`, `stable_read`, `class` and `overrides` come from `specs/blocks/{name}.toml`.\n\n\
         use pemu_core::fidelity::Fidelity;\n\
         use pemu_core::regstore::{{FieldAccess, RegSpec}};\n\n\
         use super::{{{imports}}};\n\n\
         /// Number of registers of the block.\n\
         pub const REG_COUNT: usize = {regs};\n\n\
         /// Window size of the `{name}` row of `c3_devices!`.\n\
         pub const BLOCK_SIZE: u32 = {size:#X};\n\n\
         /// Every register of the block, in offset order.\n\
         pub static REGS: [RegSpec; REG_COUNT] = [\n",
        name = b.name,
        regs = b.regs.len(),
        fields = b.field_count(),
        sources = source_doc(b),
        size = b.size,
        imports = domains
            .iter()
            .map(String::as_str)
            .chain(["field", "reg"])
            .collect::<Vec<_>>()
            .join(", "),
    );
    for r in &b.regs {
        let _ = writeln!(
            out,
            "reg(\"{}\", {:#05X}, {}, &[",
            r.name,
            r.off,
            hex(r.reset)
        );
        for f in &r.fields {
            let _ = writeln!(
                out,
                "field(\"{}\", {}, {}, FieldAccess::{}, {}),",
                f.field,
                f.shift,
                f.width,
                f.access.variant(),
                hex(r.field_reset(f))
            );
        }
        let _ = writeln!(
            out,
            "], {}, {}, Fidelity::{}, \"{}\"),",
            domain_const(r.domain),
            r.stable_read,
            r.class,
            r.cite
        );
    }
    out.push_str("];\n\n/// Index of each register in [`REGS`].\npub mod idx {\n");
    for (i, r) in b.regs.iter().enumerate() {
        let _ = writeln!(out, "pub const {}: usize = {i};", r.name);
    }
    let _ = write!(
        out,
        "}}\n\n#[cfg(test)]\nmod tests {{\n\
         #[test]\n\
         fn table_is_consistent() {{\n\
         super::super::check::table(\"{name}\", &super::REGS, super::BLOCK_SIZE);\n\
         }}\n\n\
         #[test]\n\
         fn fields_decode() {{\n\
         super::super::check::decode(\"{name}\", &super::REGS);\n\
         }}\n\n\
         #[test]\n\
         fn resets_restore_the_table() {{\n\
         super::super::check::resets(\"{name}\", &super::REGS);\n\
         }}\n\n\
         #[test]\n\
         fn access_types_behave() {{\n\
         super::super::check::access(\"{name}\", &super::REGS);\n\
         }}\n}}\n",
        name = b.name
    );
    out
}

/// Bits of one field at their register positions.
fn field_bits(f: &Row) -> u32 {
    let ones = if f.width == 32 {
        u32::MAX
    } else {
        (1u32 << f.width) - 1
    };
    ones << f.shift
}

/// Documentation of a `gen/mod.rs` domain constant, as `///` lines.
fn domain_doc(mask: u8) -> String {
    let names = |m: u8| {
        [
            (SCOPE_CHIP, "Chip"),
            (SCOPE_SYSTEM, "System"),
            (SCOPE_CORE, "Core"),
        ]
        .into_iter()
        .filter(|(bit, _)| m & bit != 0)
        .map(|(_, name)| name)
        .collect::<Vec<_>>()
        .join(" or ")
    };
    let kept = names(!mask & 0x7);
    let text = if mask == 0 {
        "Registers that no reset scope restores, from a `scopes = []` reset_domains row."
            .to_string()
    } else if kept.is_empty() {
        "Registers that every reset scope restores.".to_string()
    } else {
        format!(
            "Registers restored by a {} reset and kept across a {kept} reset.",
            names(mask)
        )
    };
    doc_lines_at("///", 0, &text)
}

/// Wraps one documentation text into comment lines of `prefix` that fit the rustfmt width, with
/// `indent` spaces of room left for the indentation rustfmt will add.
fn doc_lines_at(prefix: &str, indent: usize, text: &str) -> String {
    let mut out = String::new();
    let mut line = String::from(prefix);
    for word in text.split_whitespace() {
        if line.len() + 1 + word.len() + indent > 98 {
            let _ = writeln!(out, "{line}");
            line = String::from(prefix);
        }
        line.push(' ');
        line.push_str(word);
    }
    let _ = writeln!(out, "{line}");
    out
}

/// The `//!` paragraph naming the IDF headers a block's rows come from, with the window offset of
/// every base after the first (`import::BLOCKS`).
fn source_doc(b: &Block) -> String {
    let names: Vec<String> = b
        .headers
        .iter()
        .map(|(header, at)| {
            let path = format!("`soc/esp32c3/register/soc/{header}`");
            if *at == 0 {
                path
            } else {
                format!("{path} at offset {at:#05X} of the window")
            }
        })
        .collect();
    doc_lines_at(
        "//!",
        0,
        &format!(
            "From `specs/c3-registers.csv`, whose rows come from ESP-IDF v5.5.3 {}.",
            names.join(" and ")
        ),
    )
}

/// Hex literal of a reset value, in groups of four digits above 16 bits.
fn hex(v: u32) -> String {
    if v > 0xFFFF {
        format!("0x{:04X}_{:04X}", v >> 16, v & 0xFFFF)
    } else {
        format!("{v:#X}")
    }
}

/// Renders `gen/mod.rs` (before rustfmt): the module declarations, the documented defaults, the
/// `reg` and `field` helpers, the shared table check and the GDMA and MMU layout tests.
pub fn render_mod(blocks: &[Block]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", super::GENERATED_HEADER);
    out.push_str(MOD_DOC);
    out.push_str(
        "use pemu_core::fidelity::Fidelity;\n\
         use pemu_core::regstore::{FieldAccess, FieldSpec, RegSpec};\n\
         use pemu_core::reset::ResetDomain;\n\n",
    );
    for b in blocks {
        let _ = writeln!(
            out,
            "/// Register table of the `{}` block.\npub mod {};",
            b.name,
            b.module()
        );
    }
    let _ = writeln!(
        out,
        "\n/// The `busy-waits` table, from the `[[wait]]` rows of every block file.\n\
         pub mod {};",
        super::waits::MODULE
    );
    let _ = writeln!(
        out,
        "\n/// Fidelity classes by block offset, for the blocks with no `RegSpec` table of their\n\
         /// own (`xtask/src/codegen/classes.rs`).\n\
         pub mod {};",
        super::classes::MODULE
    );
    let mut domains: Vec<u8> = blocks
        .iter()
        .flat_map(|b| b.regs.iter().map(|r| r.domain))
        .collect();
    domains.sort_unstable();
    domains.dedup();
    out.push('\n');
    for mask in domains {
        let _ = writeln!(
            out,
            "{}pub const {}: ResetDomain = ResetDomain({mask:#X});",
            domain_doc(mask),
            domain_const(mask)
        );
    }
    out.push_str(MOD_HELPERS);
    out.push_str(MOD_CHECK);
    out.push_str(&render_anchors(blocks));
    out.push_str(MOD_LAYOUT);
    out
}

/// Reset values read by hand out of the ESP-IDF v5.5.3 headers, as `(block, register, reset,
/// citation)`. These are the one part of the generated tests that does not come from
/// `specs/c3-registers.csv`: they are typed from the header lines the citation names, so an import
/// that reads a default or a field position wrong fails the `anchors` test instead of agreeing
/// with itself. One register per shape: a whole-word default, a `bitpos` default shorter than the
/// word, a several-field register and a register with a non-zero default above bit 0.
const RESET_ANCHORS: [(&str, &str, u32, &str); 13] = [
    (
        "gpio",
        "GPIO_SIGMADELTA0",
        0x0000_FF00,
        "gpio_sd_reg.h:15,21: SD0_PRESCALE 8'hff at 8 | SD0_IN 8'h0 at 0",
    ),
    (
        "gpio",
        "GPIO_DATE",
        0x0200_6130,
        "gpio_reg.h:4566: GPIO_DATE 28'h2006130",
    ),
    (
        "i2c0",
        "I2C_DATE",
        0x2007_0201,
        "i2c_reg.h:1036: I2C_DATE 32'h20070201",
    ),
    (
        "ledc",
        "LEDC_DATE",
        0x1906_1700,
        "ledc_reg.h:1205: LEDC_DATE 32'h19061700",
    ),
    (
        "systimer",
        "SYSTIMER_DATE",
        0x0200_6171,
        "systimer_reg.h:548: SYSTIMER_DATE default 33579377",
    ),
    (
        "usj",
        "USB_SERIAL_JTAG_DATE",
        0x0200_7300,
        "usb_serial_jtag_reg.h:980: DATE default 33583872",
    ),
    (
        "intc",
        "INTERRUPT_CORE0_INTERRUPT_DATE",
        0x0200_7210,
        "interrupt_core0_reg.h:835: INTERRUPT_DATE 28'h2007210",
    ),
    (
        "xts_aes",
        "XTS_AES_DATE",
        0x2020_0623,
        "xts_aes_reg.h:114: XTS_AES_DATE default 538969635",
    ),
    (
        "spi2",
        "SPI_USER1",
        0xB841_0007,
        "spi_reg.h:313,320,327,334,342: 5'd23 at 27 | 5'h1 at 22 | 1'b1 at 16 | 8'd7 at 0",
    ),
    (
        "rtc_cntl",
        "RTC_CNTL_SWD_WPROTECT",
        0x8F1D_312A,
        "rtc_cntl_reg.h:1757: SWD_WKEY 32'h8f1d312a",
    ),
    (
        "uhci0",
        "UHCI_ESC_CONF1",
        0x00DD_DBDB,
        "uhci_reg.h:659,665,671: 8'hdd at 16 | 8'hdb at 8 | 8'hdb at 0",
    ),
    (
        "timg0",
        "TIMG_WDTCONFIG3",
        0x07FF_FFFF,
        "timer_group_reg.h:301: WDT_STG1_HOLD default 134217727",
    ),
    (
        "gdma",
        "GDMA_IN_PERI_SEL_CH0",
        0x0000_003F,
        "gdma_reg.h:1557: PERI_IN_SEL_CH0 default 63",
    ),
];

/// Renders the `anchors` test module of `gen/mod.rs`: the generated tables by block name and the
/// hand-written reset anchors of `RESET_ANCHORS`.
fn render_anchors(blocks: &[Block]) -> String {
    let mut out = String::from(
        "\n#[cfg(test)]\nmod anchors {\n\
         use pemu_core::regstore::RegSpec;\n\n\
         /// Every generated table by block name.\n",
    );
    let _ = writeln!(
        out,
        "const TABLES: [(&str, &[RegSpec]); {}] = [",
        blocks.len()
    );
    for b in blocks {
        let _ = writeln!(out, "(\"{}\", &super::{}::REGS),", b.name, b.module());
    }
    out.push_str("];\n\n");
    out.push_str(&doc_lines_at(
        "///",
        4,
        "Reset values read by hand out of the ESP-IDF v5.5.3 register headers named in the last \
         column, as `(block, register, reset, citation)`. They are typed from those header lines \
         and not derived from `specs/c3-registers.csv`, so an import that reads a default or a \
         field position wrong fails the test below instead of agreeing with itself. The list is \
         in `xtask/src/codegen/regs.rs`.",
    ));
    let _ = writeln!(
        out,
        "const ANCHORS: [(&str, &str, u32, &str); {}] = [",
        RESET_ANCHORS.len()
    );
    for (block, reg, reset, cite) in RESET_ANCHORS {
        // One line per element: rustfmt does not reflow a string literal, so no line of the
        // rendered text may be longer than the rustfmt width.
        let _ = writeln!(
            out,
            "(\n\"{block}\",\n\"{reg}\",\n{},\n\"{cite}\",\n),",
            hex(reset)
        );
    }
    out.push_str(
        "];\n\n\
         /// Every anchor register of `ANCHORS` exists in its\n\
         /// generated table and holds the reset value its IDF header states.\n\
         #[test]\n\
         fn generated_resets_match_the_idf_headers() {\n\
         for (block, name, reset, cite) in ANCHORS {\n\
         let regs = TABLES\n\
         .iter()\n\
         .find(|(b, _)| *b == block)\n\
         .unwrap_or_else(|| panic!(\"{block}: no generated table\"))\n\
         .1;\n\
         let r = regs\n\
         .iter()\n\
         .find(|r| r.name == name)\n\
         .unwrap_or_else(|| panic!(\"{block}.{name}: not in the generated table ({cite})\"));\n\
         assert_eq!(r.reset, reset, \"{block}.{name}: IDF {cite}\");\n\
         }\n\
         }\n}\n",
    );
    out
}

/// Mask of the scopes that restore every field of the register, or `None` when the fields disagree
/// or a scope's value differs from the chip reset.
fn domain_mask(fields: &[&Row]) -> Option<u8> {
    let mut mask = None;
    for f in fields {
        let mut bits = SCOPE_CHIP;
        for (value, bit) in [(f.reset_system, SCOPE_SYSTEM), (f.reset_core, SCOPE_CORE)] {
            match value {
                ScopeReset::Value(v) if v == f.reset_chip => bits |= bit,
                ScopeReset::Value(_) => return None,
                ScopeReset::Keep => {}
            }
        }
        if *mask.get_or_insert(bits) != bits {
            return None;
        }
    }
    mask
}

/// Module documentation of the generated `gen/mod.rs`.
const MOD_DOC: &str = r#"//!
//! Register tables of the `c3_devices!` blocks: one module per block with rows in
//! `specs/c3-registers.csv`, merged with `specs/blocks/<block>.toml`. `xtask/src/codegen.rs` lists
//! the blocks the importer cannot read, which have no module.
//!
//! `RegSpec::off` is the offset from the block base and `RegSpec::reset` the chip-reset value, the
//! OR of the shifted field resets; `FieldSpec::reset` is not shifted. From the block file:
//!
//! - `domain`: mask of the `ResetScope`s that restore the register (`Chip` 0x1, `System` 0x2,
//!   `Core` 0x4). UNVERIFIED encoding: the rows state kept or reset per scope, not a bit layout.
//! - `stable_read`: only such registers enter an oracle read comparison.
//! - `class`: the register's `overrides` class, else the block's class for a register a row names,
//!   else `Fidelity::U`. UNVERIFIED inheritance rule, see `xtask/src/codegen/blocks.rs`.
//!
//! An integer `overrides` value replaces a field's CSV reset, and the register reset is recomputed.

"#;

/// The `reg` and `field` helpers of the generated `gen/mod.rs`.
const MOD_HELPERS: &str = r#"
/// One `FieldSpec` row of a generated table; `reset` is the field value, not shifted.
pub const fn field(
    name: &'static str,
    shift: u8,
    width: u8,
    access: FieldAccess,
    reset: u32,
) -> FieldSpec {
    FieldSpec { name, shift, width, access, reset }
}

/// One `RegSpec` row of a generated table. `domain`, `stable_read` and `class` come
/// from `specs/blocks/<block>.toml` (see the module documentation).
///
/// One argument per `RegSpec` field, so the generated rows stay one call each; the struct it
/// builds has the same eight fields.
#[allow(clippy::too_many_arguments)]
pub const fn reg(
    name: &'static str,
    off: u16,
    reset: u32,
    fields: &'static [FieldSpec],
    domain: ResetDomain,
    stable_read: bool,
    class: Fidelity,
    cite: &'static str,
) -> RegSpec {
    RegSpec {
        name,
        off,
        reset,
        fields,
        domain,
        stable_read,
        class,
        cite,
    }
}
"#;

/// The shared table check of the generated `gen/mod.rs`.
const MOD_CHECK: &str = r#"
#[cfg(test)]
pub(crate) mod check {
    use pemu_core::regstore::{
        FieldAccess, FieldSpec, RegSpec, RegStore, Size, reserved_bits, resets_in,
    };
    use pemu_core::reset::ResetScope;

    /// Table check shared by the per-block tests: every offset is word-aligned,
    /// unique, ascending and inside the `c3_devices!` window of the block; every field fits in 32
    /// bits and overlaps no other field of its register; every register reset is the OR of its
    /// shifted field resets.
    pub fn table(block: &str, regs: &[RegSpec], block_size: u32) {
        let row = crate::periph::BLOCKS
            .iter()
            .find(|b| b.name == block)
            .unwrap_or_else(|| panic!("{block}: not a row of the c3_devices! table"));
        assert_eq!(row.size, block_size, "{block}: window size");
        assert!(!regs.is_empty(), "{block}: empty table");
        let mut prev = None;
        for r in regs {
            assert!(
                r.off.is_multiple_of(4),
                "{block}.{}: offset not word-aligned",
                r.name
            );
            assert!(
                u32::from(r.off) + 4 <= block_size,
                "{block}.{}: outside the block window",
                r.name
            );
            assert!(
                prev < Some(r.off),
                "{block}.{}: offsets are not unique and ascending",
                r.name
            );
            prev = Some(r.off);
            let mut mask = 0u32;
            let mut reset = 0u32;
            for f in r.fields {
                assert!(
                    f.width > 0 && u32::from(f.shift) + u32::from(f.width) <= 32,
                    "{block}.{}.{}: outside 32 bits",
                    r.name,
                    f.name
                );
                let ones = if f.width == 32 {
                    u32::MAX
                } else {
                    (1u32 << f.width) - 1
                };
                assert!(
                    f.reset <= ones,
                    "{block}.{}.{}: reset wider than the field",
                    r.name,
                    f.name
                );
                let bits = ones << f.shift;
                assert_eq!(
                    mask & bits,
                    0,
                    "{block}.{}.{}: fields overlap",
                    r.name,
                    f.name
                );
                mask |= bits;
                reset |= f.reset << f.shift;
            }
            assert_eq!(
                reset, r.reset,
                "{block}.{}: reset is not the OR of its field resets",
                r.name
            );
        }
    }

    /// Bits of a field of `width` bits, at bit 0.
    fn ones(width: u8) -> u32 {
        if width == 32 { u32::MAX } else { (1u32 << width) - 1 }
    }

    /// Reads a field out of a register word at its own `shift` and `width`.
    fn get(word: u32, f: &FieldSpec) -> u32 {
        (word >> f.shift) & ones(f.width)
    }

    /// Writes a field into a register word, leaving every other bit alone.
    fn set(word: u32, f: &FieldSpec, v: u32) -> u32 {
        (word & !(ones(f.width) << f.shift)) | ((v & ones(f.width)) << f.shift)
    }

    /// Field-decode check shared by the per-block tests: the reset word of every
    /// register decodes back into its field reset values, and writing a probe value into one
    /// field reads back as that value and leaves every other field of the register at its reset.
    /// That is the property a `RegStore` narrow access depends on; it fails if a
    /// `shift`, a `width` or a field reset of the table is wrong.
    pub fn decode(block: &str, regs: &[RegSpec]) {
        for r in regs {
            for f in r.fields {
                assert_eq!(
                    get(r.reset, f),
                    f.reset,
                    "{block}.{}: reset does not decode to {}",
                    r.name,
                    f.name
                );
            }
            for (i, f) in r.fields.iter().enumerate() {
                // All ones, the lowest bit and an alternating pattern, each cut to the width.
                for probe in [ones(f.width), 1, 0xAAAA_AAAA & ones(f.width)] {
                    let word = set(r.reset, f, probe);
                    assert_eq!(
                        get(word, f),
                        probe,
                        "{block}.{}.{}: {probe:#X} does not read back",
                        r.name,
                        f.name
                    );
                    for (j, other) in r.fields.iter().enumerate().filter(|(j, _)| *j != i) {
                        assert_eq!(
                            get(word, other),
                            other.reset,
                            "{block}.{}: writing {} changed field {} (index {j})",
                            r.name,
                            f.name,
                            other.name
                        );
                    }
                }
            }
        }
    }

    /// Reset check shared by the per-block tests: a fresh store starts at the reset values, and
    /// with every register holding the complement of its reset value a reset of one `ResetScope`
    /// restores exactly the registers whose `domain` holds that scope and leaves every other
    /// register alone. A register with an empty domain (a `scopes = []` row of the block file) is
    /// kept by every scope.
    pub fn resets<const N: usize>(block: &str, regs: &'static [RegSpec; N]) {
        let fresh = RegStore::new(regs);
        for (i, r) in regs.iter().enumerate() {
            assert_eq!(
                r.domain.0 & !0x7,
                0,
                "{block}.{}: domain outside the three reset scopes",
                r.name
            );
            assert_eq!(
                fresh.get(i),
                r.reset,
                "{block}.{}: a fresh store does not hold the reset value",
                r.name
            );
        }
        for scope in [ResetScope::Chip, ResetScope::System, ResetScope::Core] {
            let mut s = RegStore::new(regs);
            for (i, r) in regs.iter().enumerate() {
                s.set(i, !r.reset);
            }
            s.reset(scope);
            for (i, r) in regs.iter().enumerate() {
                let want = if resets_in(r.domain, scope) {
                    r.reset
                } else {
                    !r.reset
                };
                assert_eq!(s.get(i), want, "{block}.{}: after a {scope:?} reset", r.name);
            }
        }
    }

    /// Access check shared by the per-block tests: one read and one write per field access type
    /// of the block, each checked against the `access` column of the spec row (the semantics
    /// table of `pemu_core::regstore`), plus the reserved bits of every register with fields.
    /// Each step starts from a fresh store and writes only the bits of the field under test, so
    /// no other field of the register is triggered.
    pub fn access<const N: usize>(block: &str, regs: &'static [RegSpec; N]) {
        for (i, r) in regs.iter().enumerate() {
            let reserved = reserved_bits(r);
            if reserved != 0 {
                let mut s = RegStore::new(regs);
                let d = s.write(i, 0, Size::B4, u32::MAX);
                assert_eq!(
                    d.after & reserved,
                    r.reset & reserved,
                    "{block}.{}: a write changed a reserved bit",
                    r.name
                );
                assert_eq!(
                    s.read(i, 0, Size::B4) & reserved,
                    r.reset & reserved,
                    "{block}.{}: a reserved bit does not read its reset value",
                    r.name
                );
            }
            for f in r.fields {
                let bits = ones(f.width) << f.shift;
                let hidden = matches!(f.access, FieldAccess::Wo | FieldAccess::Wt);
                let mut s = RegStore::new(regs);
                let want = if hidden { 0 } else { f.reset };
                assert_eq!(
                    (s.read(i, 0, Size::B4) & bits) >> f.shift,
                    want,
                    "{block}.{}.{}: read after reset",
                    r.name,
                    f.name
                );
                if matches!(f.access, FieldAccess::Rc) {
                    assert_eq!(
                        s.read(i, 0, Size::B4) & bits,
                        0,
                        "{block}.{}.{}: RC does not clear on read",
                        r.name,
                        f.name
                    );
                }
                let mut s = RegStore::new(regs);
                let before = s.get(i);
                match f.access {
                    FieldAccess::Rw | FieldAccess::Wo => {
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            d.after & bits,
                            bits,
                            "{block}.{}.{}: the written bits are not stored",
                            r.name,
                            f.name
                        );
                        assert_eq!(
                            (d.w1c, d.triggers),
                            (0, 0),
                            "{block}.{}.{}: unexpected delta",
                            r.name,
                            f.name
                        );
                        assert_eq!(
                            s.read(i, 0, Size::B4) & bits,
                            if hidden { 0 } else { bits },
                            "{block}.{}.{}: read back after the write",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::Ro => {
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            d.after & bits,
                            before & bits,
                            "{block}.{}.{}: RO did not ignore the write",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::W1c => {
                        s.set(i, before | bits);
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            (d.after & bits, d.w1c & bits),
                            (0, bits),
                            "{block}.{}.{}: a written 1 does not clear",
                            r.name,
                            f.name
                        );
                        s.set(i, before | bits);
                        let d = s.write(i, 0, Size::B4, 0);
                        assert_eq!(
                            d.after & bits,
                            bits,
                            "{block}.{}.{}: a written 0 does not keep",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::W1s => {
                        s.set(i, before & !bits);
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            (d.after & bits, d.triggers),
                            (bits, 0),
                            "{block}.{}.{}: a written 1 does not set",
                            r.name,
                            f.name
                        );
                        let d = s.write(i, 0, Size::B4, 0);
                        assert_eq!(
                            d.after & bits,
                            bits,
                            "{block}.{}.{}: a written 0 does not keep",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::Wt => {
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            (d.triggers & bits, d.after & bits),
                            (bits, before & bits),
                            "{block}.{}.{}: WT stored a bit or did not trigger",
                            r.name,
                            f.name
                        );
                        assert_eq!(
                            s.read(i, 0, Size::B4) & bits,
                            0,
                            "{block}.{}.{}: WT does not read 0",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::Sc => {
                        s.set(i, before & !bits);
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            (d.after & bits, d.triggers & bits),
                            (bits, bits),
                            "{block}.{}.{}: SC does not stay set until its effect completes",
                            r.name,
                            f.name
                        );
                        s.clear_sc(i, bits);
                        assert_eq!(
                            s.get(i) & bits,
                            0,
                            "{block}.{}.{}: clear_sc did not clear the bits",
                            r.name,
                            f.name
                        );
                    }
                    FieldAccess::Rc => {
                        s.set(i, before | bits);
                        assert_eq!(
                            s.read(i, 0, Size::B4) & bits,
                            bits,
                            "{block}.{}.{}: RC does not return the latched bits",
                            r.name,
                            f.name
                        );
                        s.set(i, before | bits);
                        let d = s.write(i, 0, Size::B4, bits);
                        assert_eq!(
                            d.after & bits,
                            bits,
                            "{block}.{}.{}: RC did not ignore the write",
                            r.name,
                            f.name
                        );
                    }
                }
            }
        }
    }
}
"#;

/// The GDMA and MMU layout tests of the generated `gen/mod.rs`.
const MOD_LAYOUT: &str = r#"
#[cfg(test)]
mod layout {
    /// The C3 GDMA layout. GDMA has 3 channels; the four interrupt registers of channel `ch` sit
    /// at `ch * 0x10` and its receive and transmit register blocks at 0x070 and 0x0D0 plus
    /// `ch * 0xC0`, OUT_LINK at 0x0E0 plus `ch * 0xC0`, and the trigger peripheral numbers
    /// written into PERI_SEL are SPI2 0, I2S0 3 and SHA 7 (IDF v5.5.3
    /// `soc/esp32c3/register/soc/gdma_reg.h`, in particular `gdma_reg.h:1705`
    /// GDMA_OUT_LINK_CH0_REG at 0xe0, and
    /// `soc/esp32c3/include/soc/gdma_channel.h:11,13,15` SOC_GDMA_TRIG_PERIPH_SPI2,
    /// SOC_GDMA_TRIG_PERIPH_I2S0 and SOC_GDMA_TRIG_PERIPH_SHA0).
    #[test]
    fn gdma_c3_channel_layout() {
        use super::regs_gdma::{BLOCK_SIZE, REGS};

        /// Channels of the C3 GDMA.
        const CHANNELS: u16 = 3;
        /// Stride between the receive or transmit register block of two channels.
        const CHANNEL_STRIDE: u16 = 0xC0;
        /// IDF `gdma_channel.h` trigger peripheral numbers of the three GDMA users.
        const TRIGGERS: [(&str, u32); 3] = [("SPI2", 0), ("I2S0", 3), ("SHA", 7)];

        let off = |name: &str| REGS.iter().find(|r| r.name == name).map(|r| r.off);
        let mut per_channel = (0, 0, 0);
        for r in &REGS {
            let Some(stem) = r.name.strip_suffix("_CH0") else {
                // Channels 1 and up are checked against their channel 0 register below.
                let other = (1..CHANNELS).any(|ch| r.name.ends_with(&format!("_CH{ch}")));
                assert!(
                    other || ["GDMA_MISC_CONF", "GDMA_DATE"].contains(&r.name),
                    "{}: neither a channel register nor a shared one",
                    r.name
                );
                continue;
            };
            let stride = if r.off < 0x40 {
                per_channel.0 += 1;
                0x10
            } else if stem.starts_with("GDMA_IN") {
                per_channel.1 += 1;
                0xC0
            } else {
                per_channel.2 += 1;
                0xC0
            };
            for ch in 1..CHANNELS {
                let name = format!("{stem}_CH{ch}");
                assert_eq!(off(&name), Some(r.off + ch * stride), "{name}");
            }
            assert_eq!(
                off(&format!("{stem}_CH{CHANNELS}")),
                None,
                "{stem}: the C3 GDMA has {CHANNELS} channels"
            );
        }
        assert_eq!(
            per_channel,
            (4, 13, 13),
            "interrupt, receive and transmit registers per channel"
        );
        assert_eq!(off("GDMA_INT_RAW_CH0"), Some(0x000));
        assert_eq!(off("GDMA_IN_CONF0_CH0"), Some(0x070));
        assert_eq!(off("GDMA_OUT_CONF0_CH0"), Some(0x0D0));
        assert_eq!(off("GDMA_OUT_LINK_CH0"), Some(0x0E0));
        assert_eq!(off("GDMA_IN_PERI_SEL_CH2"), Some(0x220));
        assert_eq!(off("GDMA_OUT_PERI_SEL_CH2"), Some(0x280));
        assert_eq!(REGS.len(), 2 + usize::from(CHANNELS) * 30);
        assert!(u32::from(off("GDMA_OUT_PERI_SEL_CH2").unwrap()) < BLOCK_SIZE);
        for ch in 0..CHANNELS {
            let base = ch * CHANNEL_STRIDE;
            assert_eq!(off(&format!("GDMA_IN_CONF0_CH{ch}")), Some(0x070 + base));
            assert_eq!(off(&format!("GDMA_OUT_CONF0_CH{ch}")), Some(0x0D0 + base));
            assert_eq!(off(&format!("GDMA_OUT_LINK_CH{ch}")), Some(0x0E0 + base));
        }

        // The trigger peripheral number of a channel goes into the PERI_SEL field of that
        // direction; every number of `TRIGGERS` has to fit in it and differ from the reset
        // value, which selects no peripheral.
        for dir in ["IN", "OUT"] {
            let sel = REGS
                .iter()
                .find(|r| r.name == format!("GDMA_{dir}_PERI_SEL_CH0"))
                .and_then(|r| r.fields.iter().find(|f| f.name.contains("PERI_")))
                .unwrap_or_else(|| panic!("GDMA_{dir}_PERI_SEL_CH0 selector field"));
            for (periph, num) in TRIGGERS {
                assert!(
                    num < (1u32 << u32::from(sel.width)),
                    "{periph} trigger {num} does not fit in {}",
                    sel.name
                );
                assert_ne!(
                    num, sel.reset,
                    "{periph} trigger {num} is the {} reset value",
                    sel.name
                );
            }
            assert_eq!(sel.shift, 0, "{}", sel.name);
            assert_eq!(sel.width, 6, "{}: IDF PERI_SEL width", sel.name);
        }
        assert_eq!(TRIGGERS.map(|(_, n)| n), [0, 3, 7], "IDF gdma_channel.h");
    }

    /// The MMU index formula and the invalid bit. An entry id is
    /// `(vaddr & 0x7FFFFF) >> 16` over 128 word entries at the `mmu` block base, and bit 8 of an
    /// entry marks it invalid, just above its 8 value bits (IDF v5.5.3
    /// `soc/esp32c3/include/soc/ext_mem_defs.h` SOC_MMU_VADDR_MASK, SOC_MMU_ENTRY_NUM,
    /// SOC_MMU_INVALID and SOC_MMU_VALID_VAL_MASK; `hal/esp32c3/include/hal/mmu_ll.h`
    /// `mmu_ll_get_entry_id`; `reg_base.h` DR_REG_MMU_TABLE).
    #[test]
    fn mmu_index_formula() {
        /// IDF SOC_MMU_VADDR_MASK: 128 entries of one 64 KB page.
        const VADDR_MASK: u32 = 0x7F_FFFF;
        /// Flash MMU page size, 64 KB.
        const PAGE_BITS: u32 = 16;
        /// IDF SOC_MMU_ENTRY_NUM.
        const ENTRY_NUM: u32 = 128;
        /// IDF SOC_MMU_INVALID.
        const INVALID: u32 = 1 << 8;
        /// IDF SOC_MMU_VALID_VAL_MASK.
        const VALID_VAL_MASK: u32 = 0xFF;

        let entry = |vaddr: u32| (vaddr & VADDR_MASK) >> PAGE_BITS;
        assert_eq!(entry(0x3C00_0000), 0, "DBUS cache base");
        assert_eq!(entry(0x4200_0000), 0, "IBUS cache base");
        assert_eq!(entry(0x3C0A_0000), 0x0A);
        assert_eq!(entry(0x4201_FFFF), 1, "any address inside a page");
        assert_eq!(entry(0x3C7F_0000), ENTRY_NUM - 1, "last entry");
        assert_eq!(entry(0x3C80_0000), 0, "the mask wraps after 8 MB");
        for i in 0..ENTRY_NUM {
            assert_eq!(entry(0x4200_0000 + (i << PAGE_BITS)), i);
        }
        assert_eq!(
            INVALID,
            VALID_VAL_MASK + 1,
            "the invalid bit sits above the value bits"
        );

        let mmu = crate::periph::BLOCKS[usize::from(crate::periph::id::MMU.0)];
        assert_eq!(mmu.base, 0x600C_5000, "IDF DR_REG_MMU_TABLE");
        assert!(
            ENTRY_NUM * 4 <= mmu.size,
            "the entry table fits in the mmu window"
        );

        let fault = super::regs_extmem::REGS
            .iter()
            .find(|r| r.name == "EXTMEM_CACHE_MMU_FAULT_CONTENT")
            .expect("EXTMEM_CACHE_MMU_FAULT_CONTENT");
        assert_eq!(fault.off, 0x0A0);
        let content = fault
            .fields
            .iter()
            .find(|f| f.name == "EXTMEM_CACHE_MMU_FAULT_CONTENT")
            .expect("EXTMEM_CACHE_MMU_FAULT_CONTENT field");
        assert_eq!(content.shift, 0);
        assert!(
            (INVALID | VALID_VAL_MASK) < (1u32 << u32::from(content.width)),
            "the fault content field holds a whole MMU entry"
        );
    }
}
"#;
