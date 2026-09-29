//! Builds `specs/irq-sources.toml` from ESP-IDF v5.5.3: enumerator values of `periph_interrupt_t`
//! (`soc/interrupts.h`) and the offsets of the `INTERRUPT_CORE0_*_MAP_REG`
//! registers (`interrupt_core0_reg.h`), paired by name through `PAIRS`, never by position.

use super::idf;
use super::irq::{self, Source};

/// IDF header with the `periph_interrupt_t` enum, relative to the IDF root.
pub const INTERRUPTS_H: &str = "components/soc/esp32c3/include/soc/interrupts.h";
/// IDF header with the MAP registers, relative to the IDF root.
pub const INTC_REG_H: &str = "components/soc/esp32c3/register/soc/interrupt_core0_reg.h";

/// `(constant, IDF enumerator suffix before _SOURCE, MAP register infix)`. The constant is the
/// short name of the generated table; `SPI_MEM_REJECT` is named as `irq_numbering_matches_map_offsets`
/// names it, and the timer group rows drop `_LEVEL`. The table order carries no meaning: rows are matched by name.
const PAIRS: [(&str, &str, &str); irq::SOURCE_COUNT] = [
    ("WIFI_MAC", "WIFI_MAC_INTR", "MAC_INTR"),
    ("WIFI_MAC_NMI", "WIFI_MAC_NMI", "MAC_NMI"),
    ("WIFI_PWR", "WIFI_PWR_INTR", "PWR_INTR"),
    ("WIFI_BB", "WIFI_BB_INTR", "BB_INT"),
    ("BT_MAC", "BT_MAC_INTR", "BT_MAC_INT"),
    ("BT_BB", "BT_BB_INTR", "BT_BB_INT"),
    ("BT_BB_NMI", "BT_BB_NMI", "BT_BB_NMI"),
    ("RWBT", "RWBT_INTR", "RWBT_IRQ"),
    ("RWBLE", "RWBLE_INTR", "RWBLE_IRQ"),
    ("RWBT_NMI", "RWBT_NMI", "RWBT_NMI"),
    ("RWBLE_NMI", "RWBLE_NMI", "RWBLE_NMI"),
    ("I2C_MASTER", "I2C_MASTER", "I2C_MST_INT"),
    ("SLC0", "SLC0_INTR", "SLC0_INTR"),
    ("SLC1", "SLC1_INTR", "SLC1_INTR"),
    ("APB_CTRL", "APB_CTRL_INTR", "APB_CTRL_INTR"),
    ("UHCI0", "UHCI0_INTR", "UHCI0_INTR"),
    ("GPIO", "GPIO_INTR", "GPIO_INTERRUPT_PRO"),
    ("GPIO_NMI", "GPIO_NMI", "GPIO_INTERRUPT_PRO_NMI"),
    ("SPI1", "SPI1_INTR", "SPI_INTR_1"),
    ("SPI2", "SPI2_INTR", "SPI_INTR_2"),
    ("I2S0", "I2S0_INTR", "I2S_INT"),
    ("UART0", "UART0_INTR", "UART_INTR"),
    ("UART1", "UART1_INTR", "UART1_INTR"),
    ("LEDC", "LEDC_INTR", "LEDC_INT"),
    ("EFUSE", "EFUSE_INTR", "EFUSE_INT"),
    ("TWAI", "TWAI_INTR", "CAN_INT"),
    ("USB_SERIAL_JTAG", "USB_SERIAL_JTAG_INTR", "USB_INTR"),
    ("RTC_CORE", "RTC_CORE_INTR", "RTC_CORE_INTR"),
    ("RMT", "RMT_INTR", "RMT_INTR"),
    ("I2C_EXT0", "I2C_EXT0_INTR", "I2C_EXT0_INTR"),
    ("TIMER1", "TIMER1_INTR", "TIMER_INT1"),
    ("TIMER2", "TIMER2_INTR", "TIMER_INT2"),
    ("TG0_T0", "TG0_T0_LEVEL_INTR", "TG_T0_INT"),
    ("TG0_WDT", "TG0_WDT_LEVEL_INTR", "TG_WDT_INT"),
    ("TG1_T0", "TG1_T0_LEVEL_INTR", "TG1_T0_INT"),
    ("TG1_WDT", "TG1_WDT_LEVEL_INTR", "TG1_WDT_INT"),
    ("CACHE_IA", "CACHE_IA_INTR", "CACHE_IA_INT"),
    (
        "SYSTIMER_TARGET0",
        "SYSTIMER_TARGET0_INTR",
        "SYSTIMER_TARGET0_INT",
    ),
    (
        "SYSTIMER_TARGET1",
        "SYSTIMER_TARGET1_INTR",
        "SYSTIMER_TARGET1_INT",
    ),
    (
        "SYSTIMER_TARGET2",
        "SYSTIMER_TARGET2_INTR",
        "SYSTIMER_TARGET2_INT",
    ),
    (
        "SPI_MEM_REJECT",
        "SPI_MEM_REJECT_CACHE_INTR",
        "SPI_MEM_REJECT_INTR",
    ),
    (
        "ICACHE_PRELOAD0",
        "ICACHE_PRELOAD0_INTR",
        "ICACHE_PRELOAD_INT",
    ),
    ("ICACHE_SYNC0", "ICACHE_SYNC0_INTR", "ICACHE_SYNC_INT"),
    ("APB_ADC", "APB_ADC_INTR", "APB_ADC_INT"),
    ("DMA_CH0", "DMA_CH0_INTR", "DMA_CH0_INT"),
    ("DMA_CH1", "DMA_CH1_INTR", "DMA_CH1_INT"),
    ("DMA_CH2", "DMA_CH2_INTR", "DMA_CH2_INT"),
    ("RSA", "RSA_INTR", "RSA_INT"),
    ("AES", "AES_INTR", "AES_INT"),
    ("SHA", "SHA_INTR", "SHA_INT"),
    ("FROM_CPU_INTR0", "FROM_CPU_INTR0", "CPU_INTR_FROM_CPU_0"),
    ("FROM_CPU_INTR1", "FROM_CPU_INTR1", "CPU_INTR_FROM_CPU_1"),
    ("FROM_CPU_INTR2", "FROM_CPU_INTR2", "CPU_INTR_FROM_CPU_2"),
    ("FROM_CPU_INTR3", "FROM_CPU_INTR3", "CPU_INTR_FROM_CPU_3"),
    ("ASSIST_DEBUG", "ASSIST_DEBUG_INTR", "ASSIST_DEBUG_INTR"),
    (
        "DMA_APBPERI_PMS",
        "DMA_APBPERI_PMS_INTR",
        "DMA_APBPERI_PMS_MONITOR_VIOLATE_INTR",
    ),
    (
        "CORE0_IRAM0_PMS",
        "CORE0_IRAM0_PMS_INTR",
        "CORE_0_IRAM0_PMS_MONITOR_VIOLATE_INTR",
    ),
    (
        "CORE0_DRAM0_PMS",
        "CORE0_DRAM0_PMS_INTR",
        "CORE_0_DRAM0_PMS_MONITOR_VIOLATE_INTR",
    ),
    (
        "CORE0_PIF_PMS",
        "CORE0_PIF_PMS_INTR",
        "CORE_0_PIF_PMS_MONITOR_VIOLATE_INTR",
    ),
    (
        "CORE0_PIF_PMS_SIZE",
        "CORE0_PIF_PMS_SIZE_INTR",
        "CORE_0_PIF_PMS_MONITOR_VIOLATE_SIZE_INTR",
    ),
    (
        "BAK_PMS_VIOLATE",
        "BAK_PMS_VIOLATE_INTR",
        "BACKUP_PMS_VIOLATE_INTR",
    ),
    (
        "CACHE_CORE0_ACS",
        "CACHE_CORE0_ACS_INTR",
        "CACHE_CORE0_ACS_INT",
    ),
];

/// Builds and validates the source rows from the text of the two IDF headers.
pub fn build(interrupts_h: &str, intc_reg_h: &str) -> Result<Vec<Source>, String> {
    let items = idf::parse_enum(interrupts_h, "periph_interrupt_t")?;
    let regs = idf::parse_register_header(intc_reg_h)?;
    let max = items
        .iter()
        .find(|i| i.name == "ETS_MAX_INTR_SOURCE")
        .ok_or("ETS_MAX_INTR_SOURCE not found")?;
    if usize::try_from(max.value).ok() != Some(irq::SOURCE_COUNT) {
        return Err(format!(
            "ETS_MAX_INTR_SOURCE is {}, expected {}",
            max.value,
            irq::SOURCE_COUNT
        ));
    }
    let sources_in_enum: Vec<_> = items
        .iter()
        .filter(|i| !i.alias && i.name != "ETS_MAX_INTR_SOURCE")
        .collect();
    let mut sources = Vec::new();
    for (name, ets, map) in PAIRS {
        let ets = format!("ETS_{ets}_SOURCE");
        let map_reg = format!("INTERRUPT_CORE0_{map}_MAP_REG");
        let item = sources_in_enum
            .iter()
            .find(|i| i.name == ets)
            .ok_or_else(|| format!("{name}: enumerator {ets} not found"))?;
        let reg = regs
            .iter()
            .find(|r| r.name == map_reg)
            .ok_or_else(|| format!("{name}: register {map_reg} not found"))?;
        sources.push(Source {
            name: name.to_string(),
            number: u8::try_from(item.value).map_err(|_| format!("{ets}: value out of range"))?,
            map_off: reg.offset,
            idf: ets,
            map_reg,
            cite: format!(
                "IDF v5.5.3 soc/esp32c3/include/soc/interrupts.h:{} and \
                 soc/esp32c3/register/soc/interrupt_core0_reg.h:{}",
                item.line, reg.line
            ),
        });
    }
    if let Some(extra) = sources_in_enum
        .iter()
        .find(|i| !sources.iter().any(|s| s.idf == i.name))
    {
        return Err(format!("enumerator {} has no PAIRS row", extra.name));
    }
    sources.sort_by_key(|s| s.number);
    irq::validate(&sources)?;
    Ok(sources)
}
