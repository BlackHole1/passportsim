//! `cargo xtask codegen import`: rebuilds `specs/c3-registers.csv` and `specs/irq-sources.toml`
//! from the ESP-IDF v5.5.3 register headers. The import rules are in the module documentation of
//! `codegen.rs`, the columns in `specs/README.md` section 2.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::access::map_idf;
use super::csv::{self, Row, ScopeReset};
use super::idf::{self, RegDef};
use super::import_irq;
use super::irq;

/// IDF directory of the C3 register headers, relative to the IDF root.
pub const REG_DIR: &str = "components/soc/esp32c3/register/soc";

/// One `c3_devices!` block of the import: name, window size and the IDF headers its rows come
/// from, each with the offset of its own base inside the window.
pub type Block = (&'static str, u32, &'static [(&'static str, u32)]);

/// The blocks in `c3_devices!` table order; a header's offsets are added to its
/// base's offset in the window. Multi-instance headers (`uart_reg.h`, `spi_mem_reg.h`,
/// `timer_group_reg.h`) give each instance the same layout. `apb_ctrl` uses `syscon_reg.h`, the
/// current name of `apb_ctrl_reg.h` (`reg_base.h`: "Old name for SYSCON"). `gpio` holds
/// `DR_REG_GPIO_BASE` at 0 and `DR_REG_GPIO_SD_BASE` at 0xF00 (`SDM = 0x60004f00` of
/// `soc/esp32c3/ld/esp32c3.peripherals.ld:11`).
pub const BLOCKS: [Block; 26] = [
    ("uart0", 0x1000, &[("uart_reg.h", 0)]),
    ("spi1", 0x1000, &[("spi_mem_reg.h", 0)]),
    ("spi0", 0x1000, &[("spi_mem_reg.h", 0)]),
    (
        "gpio",
        0x1000,
        &[("gpio_reg.h", 0), ("gpio_sd_reg.h", 0xF00)],
    ),
    ("rtc_cntl", 0x800, &[("rtc_cntl_reg.h", 0)]),
    ("efuse", 0x800, &[("efuse_reg.h", 0)]),
    ("uart1", 0x1000, &[("uart_reg.h", 0)]),
    ("i2c0", 0x1000, &[("i2c_reg.h", 0)]),
    ("uhci0", 0x1000, &[("uhci_reg.h", 0)]),
    ("rmt", 0x1000, &[("rmt_reg.h", 0)]),
    ("ledc", 0x1000, &[("ledc_reg.h", 0)]),
    ("timg0", 0x1000, &[("timer_group_reg.h", 0)]),
    ("timg1", 0x1000, &[("timer_group_reg.h", 0)]),
    ("systimer", 0x1000, &[("systimer_reg.h", 0)]),
    ("spi2", 0x1000, &[("spi_reg.h", 0)]),
    ("apb_ctrl", 0x1000, &[("syscon_reg.h", 0)]),
    ("i2s0", 0x1000, &[("i2s_reg.h", 0)]),
    ("gdma", 0x1000, &[("gdma_reg.h", 0)]),
    ("saradc", 0x1000, &[("apb_saradc_reg.h", 0)]),
    ("usj", 0x1000, &[("usb_serial_jtag_reg.h", 0)]),
    ("system", 0x1000, &[("system_reg.h", 0)]),
    ("sensitive", 0x1000, &[("sensitive_reg.h", 0)]),
    ("intc", 0x1000, &[("interrupt_core0_reg.h", 0)]),
    ("extmem", 0x1000, &[("extmem_reg.h", 0)]),
    ("xts_aes", 0x1000, &[("xts_aes_reg.h", 0)]),
    ("assist_debug", 0x1000, &[("assist_debug_reg.h", 0)]),
];

/// Blocks kept across a `ResetScope::Core` reset: a core reset resets the whole digital system
/// except the RTC sub-system (IDF `soc/reset_reasons.h`, "Terminology").
const KEEP_ON_CORE_RESET: [&str; 1] = ["rtc_cntl"];

struct Options {
    idf: PathBuf,
    check: bool,
}

fn parse_args(args: &[String]) -> Result<Options, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let mut opts = Options {
        idf: home.join("esp/esp-idf-v5.5.3"),
        check: false,
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--check" => opts.check = true,
            "--idf" => opts.idf = it.next().ok_or("--idf needs a directory")?.into(),
            other => return Err(format!("import: unknown argument `{other}`")),
        }
    }
    Ok(opts)
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Fails unless the IDF tree is v5.5.3, the version every citation names.
fn check_idf_version(idf: &Path) -> Result<(), String> {
    let text = read(&idf.join("tools/cmake/version.cmake"))?;
    for (key, want) in [("MAJOR", "5"), ("MINOR", "5"), ("PATCH", "3")] {
        let line = format!("set(IDF_VERSION_{key} {want})");
        if !text.lines().any(|l| l.trim() == line) {
            return Err(format!("{}: not ESP-IDF v5.5.3", idf.display()));
        }
    }
    Ok(())
}

/// Entry point of `cargo xtask codegen import`.
pub fn run(root: &Path, args: &[String]) -> Result<(), String> {
    let opts = parse_args(args)?;
    check_idf_version(&opts.idf)?;
    let mut names: Vec<&str> = BLOCKS
        .iter()
        .flat_map(|b| b.2.iter().map(|h| h.0))
        .collect();
    names.sort_unstable();
    names.dedup();
    let mut headers = BTreeMap::new();
    for name in names {
        let path = opts.idf.join(REG_DIR).join(name);
        let regs = idf::parse_register_header(&read(&path)?).map_err(|e| format!("{name}: {e}"))?;
        headers.insert(name.to_string(), regs);
    }
    let mut warnings = Vec::new();
    let rows = build_rows(&headers, &mut warnings)?;
    let csv_text = csv::render(&rows)?;
    csv::parse(&csv_text)?;
    let sources = import_irq::build(
        &read(&opts.idf.join(import_irq::INTERRUPTS_H))?,
        &read(&opts.idf.join(import_irq::INTC_REG_H))?,
    )?;
    for w in &warnings {
        println!("import: warning: {w}");
    }
    println!(
        "import: {} field rows, {} interrupt sources, {} warning(s)",
        rows.len(),
        sources.len(),
        warnings.len()
    );
    let outputs = [
        (PathBuf::from(csv::SPEC_PATH), csv_text),
        (PathBuf::from(irq::SPEC_PATH), irq::render_toml(&sources)),
    ];
    super::write_or_check(root, &outputs, opts.check)
}

/// Builds the field rows of every imported block.
fn build_rows(
    headers: &BTreeMap<String, Vec<RegDef>>,
    warnings: &mut Vec<String>,
) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    for (block, size, block_headers) in BLOCKS {
        // `(offset inside the block window, header, register)`: one window can hold more than one
        // IDF base (`gpio` holds the GPIO base and the sigma-delta base at 0xF00).
        let mut regs: Vec<(u32, &str, &RegDef)> = block_headers
            .iter()
            .flat_map(|(header, at)| {
                headers[*header]
                    .iter()
                    .map(move |r| (at + r.offset, *header, r))
            })
            .collect();
        regs.sort_by_key(|(off, _, _)| *off);
        for (i, (off, header, reg)) in regs.iter().enumerate() {
            let (off, header) = (*off, *header);
            let at = format!("{header}:{} {}", reg.line, reg.name);
            if i > 0 && regs[i - 1].0 == off {
                return Err(format!("{at}: offset shared with {}", regs[i - 1].2.name));
            }
            if !off.is_multiple_of(4) || off + 4 > size {
                return Err(format!("{at}: offset {off:#x} outside the {block} window"));
            }
            if reg.fields.is_empty() {
                // FIFO windows (`I2C_TXFIFO_START_ADDR_REG`, `RMT_CH0DATA_REG`) carry no field
                // comment; they get no row and are listed by the import.
                warnings.push(format!("{at}: no field layout in IDF; no row"));
                continue;
            }
            let mut fields: Vec<_> = reg.fields.iter().collect();
            fields.sort_by_key(|f| f.shift);
            let mut used = 0u64;
            for f in fields {
                let mask = ((1u64 << f.width) - 1) << f.shift;
                if used & mask != 0 {
                    return Err(format!("{at}: field {} overlaps another field", f.name));
                }
                used |= mask;
                let mapped = map_idf(&f.access).map_err(|e| format!("{at}.{}: {e}", f.name))?;
                let mut note: Vec<String> = Vec::new();
                if !mapped.exact {
                    note.push(mapped.reason.to_string());
                }
                note.extend(f.quirk.map(str::to_string));
                let cite = format!("IDF v5.5.3 soc/esp32c3/register/soc/{header}:{}", f.line);
                let core = if KEEP_ON_CORE_RESET.contains(&block) {
                    ScopeReset::Keep
                } else {
                    ScopeReset::Value(f.default)
                };
                rows.push(Row {
                    block: block.to_string(),
                    register: reg
                        .name
                        .strip_suffix("_REG")
                        .unwrap_or(&reg.name)
                        .to_string(),
                    offset: off,
                    field: f.name.clone(),
                    shift: f.shift,
                    width: f.width,
                    access: mapped.access,
                    exact: mapped.exact,
                    reset_chip: f.default,
                    reset_system: ScopeReset::Value(f.default),
                    reset_core: core,
                    idf_access: f.access.clone(),
                    cite,
                    note: note.join("; "),
                });
            }
        }
    }
    Ok(rows)
}
