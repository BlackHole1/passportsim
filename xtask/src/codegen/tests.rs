//! Tests of `cargo xtask codegen`.

use std::path::Path;

use super::access::{Access, map_idf};
use super::blocks;
use super::csv::{self, Row, ScopeReset};
use super::idf::{parse_default, parse_enum, parse_register_header};
use super::irq::{self, PINNED, SOURCE_COUNT, Source};
use super::regs;
use super::{format_rust, import, import_irq, workspace_root};

#[test]
fn defaults_cover_every_header_notation() {
    assert_eq!(parse_default("5'd0", 5), Ok(0));
    assert_eq!(parse_default("8'h80", 8), Ok(0x80));
    assert_eq!(parse_default("1'b1", 1), Ok(1));
    assert_eq!(parse_default("~2'b0", 2), Ok(3));
    assert_eq!(parse_default("~4'b0", 4), Ok(0xF));
    assert_eq!(parse_default("~32'b0", 32), Ok(u32::MAX));
    assert_eq!(parse_default("0", 9), Ok(0));
    assert_eq!(parse_default("4294967295", 32), Ok(u32::MAX));
    assert_eq!(parse_default("0x10", 8), Ok(0x10));
    assert!(parse_default("2'b11", 1).is_err(), "wider than the field");
    assert!(parse_default("3'q1", 3).is_err());
}

const OLD_FORMAT: &str = "\
#define SENSITIVE_ROM_TABLE_LOCK_REG          (DR_REG_SENSITIVE_BASE + 0x000)
/* SENSITIVE_ROM_TABLE_LOCK : R/W ;bitpos:[0] ;default: 1'b0 ; */
#define SENSITIVE_ROM_TABLE_LOCK  (BIT(0))
#define SPI_MEM_WP_REG  (BIT(21))
#define SENSITIVE_X_REG          (DR_REG_SENSITIVE_BASE + 0x0A4)
/* SENSITIVE_X_PMS : R/W ;bitpos:[5:2] ;default: ~4'b0 ; */
/* SENSITIVE_X_ST : R/WTC/SS ;bitpos:[31:31] ;default: 1'h1 ; */
";

const NEW_FORMAT: &str = "\
/** TIMG_T0CONFIG_REG register
 */
#define TIMG_T0CONFIG_REG(i) (DR_REG_TIMG_BASE(i) + 0x0)
/** TIMG_T0_DIVIDER : R/W; bitpos: [28:13]; default: 1;
 *  Timer 0 clock prescaler value.
 */
#define XTS_AES_PLAIN_MEM (DR_REG_XTS_AES_BASE + 0x0)
#define USB_SERIAL_JTAG_EP1_REG (DR_REG_USB_SERIAL_JTAG_BASE + 0x1c)
/* USB_SERIAL_JTAG_RDWR_BYTE : R/W; bitpos: [8:0]; default: 0;
";

#[test]
fn register_headers_parse_in_both_comment_formats() {
    let old = parse_register_header(OLD_FORMAT).unwrap();
    assert_eq!(old.len(), 2);
    assert_eq!(
        (old[1].name.as_str(), old[1].offset, old[1].line),
        ("SENSITIVE_X_REG", 0xA4, 5)
    );
    let f = &old[1].fields;
    assert_eq!((f[0].shift, f[0].width, f[0].default), (2, 4, 0xF));
    assert_eq!(
        (f[1].shift, f[1].width, f[1].access.as_str()),
        (31, 1, "R/WTC/SS")
    );

    let new = parse_register_header(NEW_FORMAT).unwrap();
    assert_eq!(new.len(), 2);
    assert_eq!(new[0].fields[0].shift, 13);
    assert_eq!(new[0].fields[0].width, 16);
    assert_eq!(new[1].offset, 0x1C);
    assert_eq!(new[1].fields[0].width, 9);

    let orphan = "#define X_MEM (DR_REG_X_BASE + 0x0)\n/** X_F : R/W; bitpos: [0]; default: 0;\n";
    assert!(
        parse_register_header(orphan).is_err(),
        "field after a memory window"
    );
}

#[test]
fn enum_values_follow_c_rules_with_aliases_and_initializers() {
    let text = "\
typedef enum {
    ETS_A_SOURCE = 0,   /**< a, level*/
    ETS_B_SOURCE,       // b
    ETS_B_EDGE_SOURCE = ETS_B_SOURCE, /**< alias */
    ETS_C_SOURCE = 7,
    ETS_D_SOURCE,
    ETS_MAX_SOURCE,
} periph_interrupt_t;
";
    let items = parse_enum(text, "periph_interrupt_t").unwrap();
    let got: Vec<_> = items
        .iter()
        .map(|i| (i.name.as_str(), i.value, i.alias))
        .collect();
    assert_eq!(
        got,
        [
            ("ETS_A_SOURCE", 0, false),
            ("ETS_B_SOURCE", 1, false),
            ("ETS_B_EDGE_SOURCE", 1, true),
            ("ETS_C_SOURCE", 7, false),
            ("ETS_D_SOURCE", 8, false),
            ("ETS_MAX_SOURCE", 9, false),
        ]
    );
    assert_eq!(items[3].line, 5);
    assert!(parse_enum(text, "other_t").is_err());
}

#[test]
fn access_mapping_marks_inferred_types() {
    for (raw, access) in [
        ("R/W", Access::Rw),
        ("RO", Access::Ro),
        ("WO", Access::Wo),
        ("WT", Access::Wt),
        ("R/W/SC", Access::Sc),
    ] {
        let m = map_idf(raw).unwrap();
        assert_eq!((m.access, m.exact, m.reason), (access, true, ""), "{raw}");
    }
    for (raw, access) in [
        ("R/WTC/SS", Access::Ro),
        ("R/SS/WTC", Access::Ro),
        ("R/W/SS", Access::Rw),
        ("R/WC/SS", Access::W1c),
        ("R/WS/SS", Access::W1s),
        // WS with SC keeps the self-clear, as R/W/SS/SC does: eFuse CMD (`R/WS/SC`) is a seed
        // busy-wait row whose bit reads back 0.
        ("R/WS/SC", Access::Sc),
        ("R/WS/SS/SC", Access::Sc),
        ("R/SS/RC", Access::Rc),
        ("WOD", Access::Wo),
    ] {
        let m = map_idf(raw).unwrap();
        assert_eq!((m.access, m.exact), (access, false), "{raw}");
        assert!(!m.reason.is_empty() && !m.reason.contains(','), "{raw}");
    }
    assert!(map_idf("RF/WF").is_err());
    for a in Access::ALL {
        assert_eq!(Access::parse(a.as_str()), Some(a));
    }
}

fn row(field: &str, note: &str) -> Row {
    Row {
        block: "rtc_cntl".to_string(),
        register: "RTC_CNTL_STORE0".to_string(),
        offset: 0x50,
        field: field.to_string(),
        shift: 0,
        width: 32,
        access: Access::Rw,
        exact: true,
        reset_chip: 0xFFFF_FFFF,
        reset_system: ScopeReset::Value(0),
        reset_core: ScopeReset::Keep,
        idf_access: "R/W".to_string(),
        cite: "IDF v5.5.3 soc/esp32c3/register/soc/rtc_cntl_reg.h:1".to_string(),
        note: note.to_string(),
    }
}

#[test]
fn csv_rows_round_trip_and_reject_unsafe_text() {
    let rows = vec![
        row("RTC_CNTL_SCRATCH0", ""),
        row("B", "SS dropped; IDF bitpos comment disagrees"),
    ];
    let text = csv::render(&rows).unwrap();
    assert!(text.starts_with(csv::HEADER));
    assert!(text.contains(",0x050,"), "{text}");
    assert_eq!(csv::parse(&text).unwrap(), rows);
    assert!(csv::render(&[row("C", "a, b")]).is_err());
    let narrow = text.replace(",32,RW,", ",4,RW,");
    assert!(csv::parse(&narrow).is_err(), "reset wider than the field");
}

fn spec_sources() -> Vec<Source> {
    let path = workspace_root().join(irq::SPEC_PATH);
    irq::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn irq_spec_keeps_stub_names_and_plan_rows() {
    let sources = spec_sources();
    assert_eq!(sources.len(), SOURCE_COUNT);
    for (name, number) in [
        ("SPI2", 19),
        ("SYSTIMER_TARGET0", 37),
        ("FROM_CPU_INTR0", 50),
    ] {
        assert_eq!(sources[number].name, name);
    }
    for (name, number, off) in PINNED {
        let s = &sources[usize::from(number)];
        assert_eq!((s.name.as_str(), s.map_off), (name, off));
    }
}

#[test]
fn irq_validation_rejects_offset_mismatch_and_reordering() {
    let good = spec_sources();
    let mut bad = good.clone();
    bad[40].map_off = 0x0A4;
    assert!(irq::validate(&bad).is_err(), "offset / 4 != number");
    let mut swapped = good.clone();
    swapped.swap(38, 39);
    assert!(irq::validate(&swapped).is_err(), "rows out of number order");
    let mut renamed = good;
    renamed[39].name = "SYSTIMER_TARGET1".to_string();
    assert!(irq::validate(&renamed).is_err(), "duplicate name");
}

/// Builds the two IDF headers from the spec rows, with the MAP registers in reverse file order
/// and an alias enumerator, and checks the import still pairs rows by name.
#[test]
fn irq_import_pairs_by_name_not_position() {
    let sources = spec_sources();
    let mut enum_h = String::from("typedef enum {\n");
    for s in &sources {
        enum_h.push_str(&format!("    {} = {}, /**< x */\n", s.idf, s.number));
        if s.number == 32 {
            enum_h.push_str(&format!("    ETS_TG0_T0_EDGE_INTR_SOURCE = {},\n", s.idf));
        }
    }
    enum_h.push_str("    ETS_MAX_INTR_SOURCE,\n} periph_interrupt_t;\n");
    let mut reg_h = String::new();
    for s in sources.iter().rev() {
        reg_h.push_str(&format!(
            "#define {} (DR_REG_INTERRUPT_CORE0_BASE + 0x{:x})\n\
             /** X_MAP : R/W; bitpos: [4:0]; default: 0;\n",
            s.map_reg, s.map_off
        ));
    }
    let built = import_irq::build(&enum_h, &reg_h).unwrap();
    let key = |v: &[Source]| -> Vec<_> {
        v.iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.number,
                    s.map_off,
                    s.idf.clone(),
                    s.map_reg.clone(),
                )
            })
            .collect()
    };
    assert_eq!(key(&built), key(&sources));
    let crossed = reg_h
        .replace("SYSTIMER_TARGET2_INT_MAP_REG", "TMP")
        .replace(
            "SPI_MEM_REJECT_INTR_MAP_REG",
            "SYSTIMER_TARGET2_INT_MAP_REG",
        )
        .replace("TMP", "SPI_MEM_REJECT_INTR_MAP_REG");
    assert!(
        import_irq::build(&enum_h, &crossed).is_err(),
        "crossed MAP offsets"
    );
}

#[test]
fn generated_irq_source_is_up_to_date() {
    let root = workspace_root();
    let expected = format_rust(&root, &irq::render(&spec_sources())).unwrap();
    let actual = std::fs::read_to_string(root.join(irq::OUTPUT_PATH)).unwrap();
    assert!(actual.starts_with(super::GENERATED_HEADER));
    assert!(!actual.contains("8.4 pitfall"), "old stub citation");
    assert!(actual == expected, "run `cargo xtask codegen`");
}

#[test]
fn register_table_is_consistent_with_the_device_table() {
    let root = workspace_root();
    super::check_register_table(&root).unwrap();
    let table = std::fs::read_to_string(root.join("crates/pemu-soc-c3/src/periph/mod.rs")).unwrap();
    for (block, size, _) in import::BLOCKS {
        let row = format!(", {size:#x} =>");
        let found = table
            .lines()
            .any(|l| l.trim_start().starts_with(&format!("{block}:")) && l.contains(&row));
        assert!(found, "c3_devices! row for {block} with size {size:#x}");
    }
}

#[test]
fn register_table_rows_carry_access_resets_and_citations() {
    let path = workspace_root().join(csv::SPEC_PATH);
    let rows = csv::parse(&std::fs::read_to_string(path).unwrap()).unwrap();
    for r in &rows {
        assert!(
            r.cite.starts_with("IDF v5.5.3 soc/esp32c3/register/soc/"),
            "{}",
            r.field
        );
        assert!(
            r.exact || !r.note.is_empty(),
            "{} UNVERIFIED without a reason",
            r.field
        );
        let core = if r.block == "rtc_cntl" {
            ScopeReset::Keep
        } else {
            ScopeReset::Value(r.reset_chip)
        };
        assert_eq!(
            (r.reset_system, r.reset_core),
            (ScopeReset::Value(r.reset_chip), core)
        );
    }
    let find = |block: &str, field: &str| {
        rows.iter()
            .find(|r| r.block == block && r.field == field)
            .unwrap_or_else(|| panic!("{field}"))
    };
    let f = find("system", "SYSTEM_RST_EN_ASSIST_DEBUG");
    assert_eq!(
        (f.offset, f.shift, f.width, f.access, f.reset_chip),
        (0x004, 6, 1, Access::Rw, 1)
    );
    let f = find("usj", "USB_SERIAL_JTAG_IN_FIFO_CNT");
    assert_eq!((f.offset, f.shift, f.width), (0x020, 0, 2));
    let f = find("intc", "INTERRUPT_CORE0_CPU_INTR_FROM_CPU_0_MAP");
    assert_eq!((f.offset, f.width, f.access), (0x0C8, 5, Access::Rw));
    assert!(
        !rows.iter().any(|r| r.field == "SYSTEM_WIFI_CLK_EN"),
        "alias comment skipped"
    );
}

/// Re-runs the import in check mode when ESP-IDF v5.5.3 is present.
#[test]
fn import_reproduces_the_spec_files_when_inputs_exist() {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let idf = home.join("esp/esp-idf-v5.5.3");
    if !Path::new(&idf).is_dir() {
        eprintln!("skipped: ESP-IDF v5.5.3 not found");
        return;
    }
    import::run(&workspace_root(), &["--check".to_string()]).unwrap();
}

/// Field rows of one register for the grouping tests: `(field, shift, width, chip reset)`.
/// `rtc_cntl` rows keep their value across a core reset, as the imported table does.
fn reg_rows(block: &str, register: &str, offset: u32, fields: &[(&str, u8, u8, u32)]) -> Vec<Row> {
    fields
        .iter()
        .map(|(field, shift, width, reset)| Row {
            block: block.to_string(),
            register: register.to_string(),
            offset,
            field: field.to_string(),
            shift: *shift,
            width: *width,
            access: Access::Rw,
            exact: true,
            reset_chip: *reset,
            reset_system: ScopeReset::Value(*reset),
            reset_core: if block == "rtc_cntl" {
                ScopeReset::Keep
            } else {
                ScopeReset::Value(*reset)
            },
            idf_access: "R/W".to_string(),
            cite: format!("IDF v5.5.3 soc/esp32c3/register/soc/{block}_reg.h:1"),
            note: String::new(),
        })
        .collect()
}

#[test]
fn register_tables_group_rows_and_derive_resets_and_domains() {
    let mut rows = reg_rows(
        "uart0",
        "UART_CLKDIV",
        0x14,
        &[
            ("UART_CLKDIV", 0, 12, 0x2B6),
            ("UART_CLKDIV_FRAG", 20, 4, 1),
        ],
    );
    rows.extend(reg_rows(
        "rtc_cntl",
        "RTC_CNTL_STORE0",
        0x50,
        &[("RTC_CNTL_SCRATCH0", 0, 32, 0)],
    ));
    let blocks = regs::group(&rows).unwrap();
    assert_eq!(blocks.len(), 2, "blocks without rows are skipped");
    let uart = &blocks[0];
    assert_eq!(
        (uart.name, uart.size, uart.regs.len(), uart.field_count()),
        ("uart0", 0x1000, 1, 2)
    );
    assert_eq!(uart.module(), "regs_uart0");
    let clkdiv = &uart.regs[0];
    assert_eq!((clkdiv.off, clkdiv.reset), (0x14, 0x2B6 | 1 << 20));
    assert_eq!(regs::domain_const(clkdiv.domain), "DOMAIN_CHIP_SYSTEM_CORE");
    let store0 = &blocks[1].regs[0];
    assert_eq!(regs::domain_const(store0.domain), "DOMAIN_CHIP_SYSTEM");

    let text = regs::render_block(uart);
    assert!(text.starts_with(super::GENERATED_HEADER));
    for needle in [
        "pub const REG_COUNT: usize = 1;",
        "pub const BLOCK_SIZE: u32 = 0x1000;",
        "reg(\"UART_CLKDIV\", 0x014, 0x0010_02B6",
        "field(\"UART_CLKDIV\", 0, 12, FieldAccess::Rw, 0x2B6)",
        "field(\"UART_CLKDIV_FRAG\", 20, 4, FieldAccess::Rw, 0x1)",
        "DOMAIN_CHIP_SYSTEM_CORE, false, Fidelity::U, \"IDF v5.5.3 \
         soc/esp32c3/register/soc/uart0_reg.h:1\"",
        "pub const UART_CLKDIV: usize = 0;",
        "check::table(\"uart0\", &super::REGS, super::BLOCK_SIZE)",
        "check::decode(\"uart0\", &super::REGS)",
        "check::resets(\"uart0\", &super::REGS)",
        "check::access(\"uart0\", &super::REGS)",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    let module = regs::render_mod(&blocks);
    for needle in [
        "pub mod regs_uart0;",
        "pub mod regs_rtc_cntl;",
        "pub const DOMAIN_CHIP_SYSTEM: ResetDomain = ResetDomain(0x3);",
        "pub const DOMAIN_CHIP_SYSTEM_CORE: ResetDomain = ResetDomain(0x7);",
        "stable_read: bool,",
        "class: Fidelity,",
        "pub fn resets<const N: usize>(block: &str, regs: &'static [RegSpec; N])",
        "pub fn access<const N: usize>(block: &str, regs: &'static [RegSpec; N])",
        "fn gdma_c3_channel_layout()",
        "fn mmu_index_formula()",
        "(vaddr & VADDR_MASK) >> PAGE_BITS",
        // The five items of the GDMA layout check.
        "assert_eq!(off(\"GDMA_IN_CONF0_CH0\"), Some(0x070));",
        "assert_eq!(off(\"GDMA_OUT_CONF0_CH0\"), Some(0x0D0));",
        "assert_eq!(off(\"GDMA_OUT_LINK_CH0\"), Some(0x0E0));",
        "const CHANNEL_STRIDE: u16 = 0xC0;",
        "const TRIGGERS: [(&str, u32); 3] = [(\"SPI2\", 0), (\"I2S0\", 3), (\"SHA\", 7)];",
        // The hand-written reset anchors, which do not come from the CSV.
        "fn generated_resets_match_the_idf_headers()",
        "\"XTS_AES_DATE\",",
        "0x2020_0623,",
        "fn decode(block: &str, regs: &[RegSpec])",
    ] {
        assert!(module.contains(needle), "{needle}");
    }
    assert!(
        module.lines().all(|l| l.len() <= 100),
        "generated lines fit the rustfmt width"
    );
}

/// One rejection case: what it breaks and the edit that breaks it.
type BrokenCase<'a> = (&'a str, &'a dyn Fn(&mut Vec<Row>));

#[test]
fn register_table_grouping_rejects_broken_rows() {
    let ok = reg_rows(
        "uart0",
        "UART_CLKDIV",
        0x14,
        &[("A", 0, 12, 0), ("B", 20, 4, 0)],
    );
    assert!(regs::group(&ok).is_ok());
    let with = |f: &dyn Fn(&mut Vec<Row>)| {
        let mut rows = ok.clone();
        f(&mut rows);
        regs::group(&rows).err()
    };
    let cases: [BrokenCase; 8] = [
        ("field overlaps", &|r: &mut Vec<Row>| r[1].shift = 8),
        ("out of shift order", &|r: &mut Vec<Row>| r.swap(0, 1)),
        ("offset not word-aligned", &|r: &mut Vec<Row>| {
            for row in r.iter_mut() {
                row.offset = 0x16;
            }
        }),
        ("outside the block window", &|r: &mut Vec<Row>| {
            for row in r.iter_mut() {
                row.offset = 0x1000;
            }
        }),
        ("fields disagree", &|r: &mut Vec<Row>| {
            r[1].reset_core = ScopeReset::Keep;
        }),
        ("scope value differs", &|r: &mut Vec<Row>| {
            r[0].reset_system = ScopeReset::Value(1);
        }),
        ("registers out of offset order", &|r: &mut Vec<Row>| {
            r.extend(reg_rows("uart0", "UART_FIFO", 0x0, &[("C", 0, 8, 0)]));
        }),
        ("rows of one register not grouped", &|r: &mut Vec<Row>| {
            r.extend(reg_rows("uart0", "UART_INT_RAW", 0x18, &[("C", 0, 8, 0)]));
            r.extend(reg_rows("uart0", "UART_CLKDIV", 0x14, &[("D", 24, 4, 0)]));
        }),
    ];
    for (what, edit) in cases {
        assert!(with(edit).is_some(), "{what} is rejected");
    }
}

/// `cargo xtask codegen --check` covers every generated file and passes on the tree.
#[test]
fn generated_files_are_up_to_date() {
    let root = workspace_root();
    super::generate(&root, true).unwrap();
    let rows = super::check_register_table(&root).unwrap();
    let blocks = regs::group(&rows).unwrap();
    let dir = root.join(regs::OUTPUT_DIR);
    let mut files: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".rs"))
        .collect();
    files.sort();
    let mut expected: Vec<String> = blocks
        .iter()
        .map(|b| format!("{}.rs", b.module()))
        .collect();
    expected.push("mod.rs".to_string());
    for path in [super::waits::OUTPUT_PATH, super::classes::OUTPUT_PATH] {
        expected.push(
            std::path::Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
    }
    expected.sort();
    assert_eq!(
        files,
        expected,
        "every file of {} is generated",
        dir.display()
    );
    for name in &files {
        let text = std::fs::read_to_string(dir.join(name)).unwrap();
        assert!(text.starts_with(super::GENERATED_HEADER), "{name}");
    }
}

#[test]
fn check_fails_on_a_generated_file_that_differs() {
    let root = workspace_root();
    let stale = vec![(
        std::path::PathBuf::from(regs::OUTPUT_DIR).join("regs_uart0.rs"),
        "//! not what codegen renders\n".to_string(),
    )];
    assert!(super::write_or_check(&root, &stale, true).is_err());
    let absent = vec![(
        std::path::PathBuf::from(regs::OUTPUT_DIR).join("regs_twai.rs"),
        String::new(),
    )];
    assert!(super::write_or_check(&root, &absent, true).is_err());
    assert!(
        super::check_no_stale_tables(&root, &[]).is_err(),
        "a file codegen no longer produces fails the check"
    );
}

/// A block file with the smallest legal content: a header and the `"*"` reset_domains row.
const MINIMAL_BLOCK: &str = r#"schema = 1
block = "uart0"
base = 0x6000_0000
size = 0x1000
class = "B"
milestone = "M1"
provenance = "test"

[[reset_domains]]
registers = "*"
scopes = ["chip", "system", "core"]
provenance = "test"
"#;

/// The `uart0` table of the CSV, for the register and field name checks of `blocks::check_names`.
fn uart0_table() -> Vec<Row> {
    let path = workspace_root().join(csv::SPEC_PATH);
    csv::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[test]
fn block_files_parse_check_and_merge_into_the_tables() {
    let root = workspace_root();
    let specs = blocks::load(&root).unwrap();
    let devices = blocks::device_rows(&root).unwrap();
    assert_eq!(devices.len(), 42, "c3_devices! rows");
    assert_eq!(
        specs.len(),
        devices.len() + 2,
        "one file per c3_devices! row plus the flash_xmc and cw2017 chip files"
    );
    let rows = uart0_table();
    let mut tables = regs::group(&rows).unwrap();
    blocks::check(&specs, &tables, &devices).unwrap();
    blocks::apply(&mut tables, &specs).unwrap();

    let gpio = tables.iter().find(|b| b.name == "gpio").unwrap();
    let strap = gpio.regs.iter().find(|r| r.name == "GPIO_STRAP").unwrap();
    assert_eq!(
        (strap.domain, strap.stable_read, strap.class, strap.reset),
        (0x7, true, "A", 0x0A),
        "the strap override and its reset_domains row of specs/blocks/gpio.toml"
    );
    assert!(
        strap
            .cite
            .ends_with("specs/blocks/gpio.toml reset_domains, overrides, stable_read"),
        "{}",
        strap.cite
    );
    let bt = gpio
        .regs
        .iter()
        .find(|r| r.name == "GPIO_BT_SELECT")
        .unwrap();
    assert_eq!(
        (bt.domain, bt.stable_read, bt.class),
        (0x7, false, "U"),
        "a register no row of the block file names stays U"
    );
    let rnd = tables
        .iter()
        .find(|b| b.name == "apb_ctrl")
        .unwrap()
        .regs
        .iter()
        .find(|r| r.name == "SYSCON_RND_DATA")
        .unwrap();
    assert_eq!(
        (rnd.domain, rnd.class),
        (0x0, "C"),
        "the `scopes = []` row and the class C override of specs/blocks/apb_ctrl.toml"
    );
    let rtc = tables.iter().find(|b| b.name == "rtc_cntl").unwrap();
    for n in 0..8 {
        let store = rtc
            .regs
            .iter()
            .find(|r| r.name == format!("RTC_CNTL_STORE{n}"))
            .unwrap();
        assert_eq!(
            store.domain, 0x3,
            "STORE{n} is RTC-retained across a core reset, and a power-on and a SYS_ reset clear \
             it (specs/blocks/rtc_cntl.toml)"
        );
    }
    let sensitive = tables.iter().find(|b| b.name == "sensitive").unwrap();
    // Every register a row of the file names leaves U, and no other one does: the lock registers
    // are class B through their `stable_read` rows, the PMS families are class C through the
    // `registers` glob rows of the same file, and a `registers` glob never demotes a register that
    // has a row of its own (`xtask/src/codegen/blocks.rs`).
    for r in &sensitive.regs {
        let named = r.stable_read || r.name == "SENSITIVE_ROM_TABLE_LOCK";
        let glob = r.name.starts_with("SENSITIVE_INTERNAL_SRAM_USAGE_")
            || r.name.starts_with("SENSITIVE_CORE_X_")
            || r.name.starts_with("SENSITIVE_CORE_0_");
        let want = if named {
            "B"
        } else if glob {
            "C"
        } else {
            "U"
        };
        assert_eq!(r.class, want, "{}", r.name);
    }
    let classed = sensitive.regs.iter().filter(|r| r.class != "U").count();
    assert!(
        classed < sensitive.regs.len(),
        "the `register = \"*\"` overrides row must not class all {} SENSITIVE registers, \
         or the strict fidelity gate can never flag a first touch",
        sensitive.regs.len()
    );
}

/// An `overrides` row with `register = "*"` states a block-wide rule in prose.
/// It classes no register, and codegen rejects an integer `value` on it, because such a value would
/// silently belong to no field.
#[test]
fn a_star_overrides_row_states_prose_and_classes_nothing() {
    let prose = format!(
        "{MINIMAL_BLOCK}\n[[overrides]]\nregister = \"*\"\nfield = \"lock bits\"\n\
         value = \"a write to a locked group is ignored\"\nclass = \"A\"\nprovenance = \"t\"\n"
    );
    let spec = blocks::parse("uart0.toml", &prose).unwrap();
    let rows = uart0_table();
    let mut tables = regs::group(&rows).unwrap();
    let mut specs = blocks::load(&workspace_root()).unwrap();
    *specs.iter_mut().find(|s| s.name == "uart0").unwrap() = spec;
    blocks::apply(&mut tables, &specs).unwrap();
    let uart0 = tables.iter().find(|b| b.name == "uart0").unwrap();
    assert!(
        uart0.regs.iter().all(|r| r.class == "U"),
        "the `*` row classed a register of the block"
    );

    let with_value = prose.replacen(
        "value = \"a write to a locked group is ignored\"",
        "value = 0x1",
        1,
    );
    let err = blocks::parse("uart0.toml", &with_value).unwrap_err();
    assert!(err.contains("cannot carry an integer `value`"), "{err}");
}

/// The merged `busy-waits` table keeps every column of every `[[wait]]` row, so the block
/// packages and the hang detector can read them.
#[test]
fn the_busy_wait_table_keeps_every_row_and_column() {
    let specs = blocks::load(&workspace_root()).unwrap();
    let rows: usize = specs.iter().map(|s| s.waits.len()).sum();
    let text = super::waits::render(&specs);
    assert_eq!(
        text.matches("    WaitSpec {").count(),
        rows,
        "one initializer per wait row"
    );
    assert!(
        text.contains(&format!("pub const WAIT_COUNT: usize = {rows};")),
        "{rows}"
    );
    assert!(text.contains(&format!(
        "pub const SEED_COUNT: usize = {};",
        blocks::SEED_WAITS
    )));
    for column in [
        "id: \"spi2.cmd_update\"",
        "kind: WaitKind::Wait",
        "seed: true",
        "block: \"spi2\"",
        "register: \"SPI_CMD\"",
        "field: \"SPI_UPDATE\"",
        "trigger: \"write SPI_UPDATE = 1\"",
        "expect: \"read SPI_UPDATE == 0\"",
        "within: Within::All(\"same_access\")",
        "polled_at: &[\"spi_ll_apply_config\"]",
        "images: &[\"official\", \"pk\"]",
        "milestone: \"M4\"",
    ] {
        assert!(text.contains(column), "the table drops `{column}`");
    }
    assert!(
        text.contains("kind: WaitKind::Tripwire"),
        "the seed tripwire row"
    );
    assert!(
        text.contains("within: Within::PerProfile { fast: \"0ms\", device:"),
        "the per-profile bound of the flash WIP row"
    );
    assert!(
        text.contains("block: \"flash_xmc\""),
        "a chip file's wait row belongs to the table too"
    );
}

#[test]
fn block_file_rejects_a_bad_header() {
    assert!(blocks::parse("uart0.toml", MINIMAL_BLOCK).is_ok());
    for (what, text) in [
        (
            "unknown key",
            MINIMAL_BLOCK.replacen("class =", "klass =", 1),
        ),
        (
            "schema must be 1",
            MINIMAL_BLOCK.replacen("schema = 1", "schema = 2", 1),
        ),
        (
            "not A, B, C or U",
            MINIMAL_BLOCK.replacen(r#"class = "B""#, r#"class = "Z""#, 1),
        ),
        (
            "missing string `provenance`",
            MINIMAL_BLOCK.replacen("provenance = \"test\"\n", "", 1),
        ),
        (
            "not M<n> or LATER",
            MINIMAL_BLOCK.replacen(r#"milestone = "M1""#, r#"milestone = "soon""#, 1),
        ),
    ] {
        let err = blocks::parse("uart0.toml", &text).unwrap_err();
        assert!(err.contains(what), "expected `{what}`, got `{err}`");
    }
    let err = blocks::parse("gpio.toml", MINIMAL_BLOCK).unwrap_err();
    assert!(err.contains("file name does not match"), "{err}");
}

#[test]
fn block_file_rejects_duplicate_rows_and_a_missing_row_provenance() {
    let dup_reset = format!(
        "{MINIMAL_BLOCK}\n[[reset_domains]]\nregisters = \"*\"\nscopes = []\nprovenance = \"t\"\n"
    );
    let err = blocks::parse("uart0.toml", &dup_reset).unwrap_err();
    assert!(err.contains("two reset_domains rows"), "{err}");

    let stable = "\n[[stable_read]]\nregister = \"UART_STATUS\"\nprovenance = \"t\"\n";
    let dup_stable = format!("{MINIMAL_BLOCK}{stable}{stable}");
    let err = blocks::parse("uart0.toml", &dup_stable).unwrap_err();
    assert!(err.contains("two stable_read rows"), "{err}");

    let no_prov = format!("{MINIMAL_BLOCK}\n[[stable_read]]\nregister = \"UART_STATUS\"\n");
    let err = blocks::parse("uart0.toml", &no_prov).unwrap_err();
    assert!(err.contains("missing key `provenance`"), "{err}");

    let wait = |id: &str| {
        format!(
            "\n[[wait]]\nid = \"{id}\"\nkind = \"wait\"\nseed = false\n\
             register = \"UART_STATUS\"\nfield = \"UART_TXFIFO_CNT\"\ntrigger = \"t\"\n\
             expect = \"t\"\nwithin = \"same_access\"\npolled_at = []\nimages = []\n\
             milestone = \"M1\"\nprovenance = \"t\"\n"
        )
    };
    let one = format!("{MINIMAL_BLOCK}{}", wait("uart0.a"));
    assert!(blocks::parse("uart0.toml", &one).is_ok());
    let two = format!("{MINIMAL_BLOCK}{}{}", wait("uart0.a"), wait("uart0.a"));
    let err = blocks::parse("uart0.toml", &two).unwrap_err();
    assert!(err.contains("two wait rows with id"), "{err}");
}

#[test]
fn block_file_rejects_a_register_of_another_block_and_an_unknown_field() {
    let rows = uart0_table();
    let tables = regs::group(&rows).unwrap();
    let uart0 = tables.iter().find(|b| b.name == "uart0").unwrap();

    let other =
        format!("{MINIMAL_BLOCK}\n[[stable_read]]\nregister = \"SPI_CMD\"\nprovenance = \"t\"\n");
    let spec = blocks::parse("uart0.toml", &other).unwrap();
    let err = blocks::check_names(&spec, uart0).unwrap_err();
    assert!(
        err.contains("`SPI_CMD` is not a register of `uart0`"),
        "{err}"
    );

    let bad_field = format!(
        "{MINIMAL_BLOCK}\n[[overrides]]\nregister = \"UART_STATUS\"\nfield = \"SPI_UPDATE\"\n\
         value = 0x0\nclass = \"B\"\nprovenance = \"t\"\n"
    );
    let spec = blocks::parse("uart0.toml", &bad_field).unwrap();
    let err = blocks::check_names(&spec, uart0).unwrap_err();
    assert!(
        err.contains("`SPI_UPDATE` is not a field of `UART_STATUS`"),
        "{err}"
    );

    let ok = format!(
        "{MINIMAL_BLOCK}\n[[overrides]]\nregister = \"UART_STATUS\"\n\
         field = \"UART_TXFIFO_CNT, UART_RXFIFO_CNT\"\nvalue = \"prose\"\nclass = \"B\"\n\
         provenance = \"t\"\n"
    );
    let spec = blocks::parse("uart0.toml", &ok).unwrap();
    blocks::check_names(&spec, uart0).unwrap();
}

#[test]
fn block_files_pin_the_seed_wait_rows_and_the_reset_domain_of_the_csv() {
    let root = workspace_root();
    let devices = blocks::device_rows(&root).unwrap();
    let rows = uart0_table();
    let tables = regs::group(&rows).unwrap();

    let mut specs = blocks::load(&root).unwrap();
    let seed = specs
        .iter_mut()
        .flat_map(|s| &mut s.waits)
        .find(|w| w.seed)
        .unwrap();
    seed.seed = false;
    let err = blocks::check(&specs, &tables, &devices).unwrap_err();
    assert!(err.contains("the seed set has"), "{err}");

    let mut specs = blocks::load(&root).unwrap();
    let uart0 = specs.iter_mut().find(|s| s.name == "uart0").unwrap();
    uart0.resets.retain(|r| r.registers != "*");
    let err = blocks::check(&specs, &tables, &devices).unwrap_err();
    assert!(
        err.contains("no `registers = \"*\"` reset_domains row"),
        "{err}"
    );

    let mut specs = blocks::load(&root).unwrap();
    let uart0 = specs.iter_mut().find(|s| s.name == "uart0").unwrap();
    uart0.resets.iter_mut().for_each(|r| r.mask = 0x1);
    let mut tables = regs::group(&rows).unwrap();
    let err = blocks::apply(&mut tables, &specs).unwrap_err();
    assert!(
        err.contains("per-scope reset columns"),
        "a `*` row that contradicts the CSV fails: {err}"
    );
}

/// A `registers` glob classes the array it names in one row, leaves a register
/// that has a row of its own alone, and is refused when it could mean something else.
#[test]
fn a_registers_glob_classes_an_array_and_never_demotes_a_named_register() {
    let glob = format!(
        "{MINIMAL_BLOCK}\n[[stable_read]]\nregister = \"UART_INT_RAW\"\nprovenance = \"t\"\n\
         \n[[overrides]]\nregisters = \"UART_INT_*\"\n\
         value = \"the interrupt registers, stored\"\nclass = \"C\"\nprovenance = \"t\"\n"
    );
    let spec = blocks::parse("uart0.toml", &glob).unwrap();
    let rows = uart0_table();
    let mut tables = regs::group(&rows).unwrap();
    blocks::check_names(&spec, tables.iter().find(|b| b.name == "uart0").unwrap()).unwrap();
    let mut specs = blocks::load(&workspace_root()).unwrap();
    *specs.iter_mut().find(|s| s.name == "uart0").unwrap() = spec;
    blocks::apply(&mut tables, &specs).unwrap();
    let uart0 = tables.iter().find(|b| b.name == "uart0").unwrap();
    let class = |name: &str| {
        uart0
            .regs
            .iter()
            .find(|r| r.name == name)
            .unwrap_or_else(|| panic!("{name}"))
            .class
    };
    assert_eq!(class("UART_INT_ENA"), "C", "the glob classes the array");
    assert_eq!(class("UART_INT_CLR"), "C");
    assert_eq!(
        class("UART_INT_RAW"),
        "B",
        "a register with a row of its own keeps the class that row gives it"
    );
    assert_eq!(
        class("UART_CLKDIV"),
        "U",
        "outside the glob, nothing changes"
    );

    for (row, want) in [
        (
            "registers = \"UART_INT_ENA\"\nvalue = \"v\"\nclass = \"C\"\nprovenance = \"t\"",
            "needs a `*`",
        ),
        (
            "registers = \"*\"\nvalue = \"v\"\nclass = \"C\"\nprovenance = \"t\"",
            "would class every register of the block",
        ),
        (
            "registers = \"UART_INT_*\"\nvalue = 1\nclass = \"C\"\nprovenance = \"t\"",
            "cannot carry an integer `value`",
        ),
        (
            "registers = \"UART_INT_*\"\nfield = \"UART_RXFIFO_FULL_INT_ENA\"\nvalue = \"v\"\n\
             class = \"C\"\nprovenance = \"t\"",
            "spans registers, so it cannot name the field",
        ),
        (
            "register = \"UART_INT_ENA\"\nregisters = \"UART_INT_*\"\nvalue = \"v\"\n\
             class = \"C\"\nprovenance = \"t\"",
            "exactly one of register, registers, command",
        ),
    ] {
        let text = format!("{MINIMAL_BLOCK}\n[[overrides]]\n{row}\n");
        let err = blocks::parse("uart0.toml", &text).unwrap_err();
        assert!(err.contains(want), "{err}");
    }

    // A glob that matches no register of the block is a typo, not a rule that classes nothing.
    let typo = format!(
        "{MINIMAL_BLOCK}\n[[overrides]]\nregisters = \"UART_NOT_A_*\"\nvalue = \"v\"\n\
         class = \"C\"\nprovenance = \"t\"\n"
    );
    let spec = blocks::parse("uart0.toml", &typo).unwrap();
    let err =
        blocks::check_names(&spec, tables.iter().find(|b| b.name == "uart0").unwrap()).unwrap_err();
    assert!(err.contains("matches no register of `uart0`"), "{err}");
}

/// An `offsets` row is how a block with no rows in
/// `specs/c3-registers.csv` carries a class, because it has no `RegSpec` table for a `class`
/// column. The rendered `gen/classes.rs` is what those models answer `Peripheral::fidelity` from.
#[test]
fn an_offsets_row_classes_a_block_that_has_no_register_table() {
    let root = workspace_root();
    let specs = blocks::load(&root).unwrap();
    let names: Vec<&str> = super::classes::classed(&specs)
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["aes", "mmu", "regi2c", "rsa", "sha"],
        "the blocks whose class cannot live in a RegSpec table"
    );
    let mmu = specs.iter().find(|s| s.name == "mmu").unwrap();
    let rows = mmu.offset_classes().unwrap();
    assert_eq!(rows.len(), 128, "one row per MMU entry");
    assert_eq!(rows.first(), Some(&(0x000, "B")));
    assert_eq!(rows.last(), Some(&(0x1FC, "B")));
    let text = super::classes::render(&specs).unwrap();
    for want in [
        "pub mod aes {",
        "pub mod mmu {",
        "pub mod rsa {",
        "pub mod sha {",
        "pub mod regi2c {",
        "pub fn class_at(off: u32) -> Fidelity {",
        "(0x01FC, Fidelity::B),",
        "(0x0040, Fidelity::C),",
    ] {
        assert!(text.contains(want), "{want}");
    }

    // The shapes codegen refuses.
    let block = MINIMAL_BLOCK.replace("block = \"uart0\"", "block = \"mmu\"");
    let block = block.replace("base = 0x6000_0000", "base = 0x600C_5000");
    for (offsets, want) in [
        ("[0x002]", "not 4-aligned"),
        ("[0x000, 0x000]", "lists one offset twice"),
        ("{ first = 0x010, last = 0x000 }", "positive `stride`"),
        ("\"0x000\"", "not a non-empty array"),
    ] {
        let text = format!(
            "{block}\n[[overrides]]\nregister = \"entries\"\noffsets = {offsets}\n\
             value = \"v\"\nclass = \"B\"\nprovenance = \"t\"\n"
        );
        let err = blocks::parse("mmu.toml", &text).unwrap_err();
        assert!(err.contains(want), "{offsets}: {err}");
    }

    // A block with CSV rows names its registers; a window is checked against the block size.
    let devices = blocks::device_rows(&root).unwrap();
    let csv_rows = uart0_table();
    let tables = regs::group(&csv_rows).unwrap();
    for (file, header, offsets, want) in [
        (
            "uart0.toml",
            MINIMAL_BLOCK.to_string(),
            "[0x000]",
            "so its rows name registers rather than raw offsets",
        ),
        (
            "mmu.toml",
            block.clone(),
            "[0x1000]",
            "outside the 0x1000-byte block window",
        ),
    ] {
        let text = format!(
            "{header}\n[[overrides]]\nregister = \"x\"\noffsets = {offsets}\n\
             value = \"v\"\nclass = \"B\"\nprovenance = \"t\"\n"
        );
        let spec = blocks::parse(file, &text).unwrap();
        let mut patched = blocks::load(&root).unwrap();
        *patched.iter_mut().find(|s| s.file == file).unwrap() = spec;
        let err = blocks::check(&patched, &tables, &devices).unwrap_err();
        assert!(err.contains(want), "{file} {offsets}: {err}");
    }
}
